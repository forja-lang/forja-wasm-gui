#![allow(dead_code)]
// Forja WASM GUI Runtime
// Dibuja interfaces Forja como DOM real (HTML accesible), sin canvas.
// No depende de winit, Xilem, Masonry ni Vello.
//
// Arquitectura:
//   - Compila código Forja a AST con `forja-syntax` (lexer + parser)
//   - Convierte AST → Layout simplificado
//   - Convierte Layout en nodos DOM (gui-dom) y los pinta como HTML
//   - Maneja eventos por delegación sobre el árbol DOM
//   - Ejecuta callbacks Forja con un evaluador tree-walking

use forja_syntax::ast::{Declaracion, Expresion, Operador, OperadorUnario};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;

/// Compila a AST usando sólo el frontend (`forja-syntax`): lexer + parser.
///
/// Antes se usaba el compilador completo, que en wasm arrastra VM, bytecode,
/// IR y codegen: el `.wasm` pesaba más y el arranque tardaba ~1 s de más.
fn compilar_a_ast(codigo: &str) -> Result<Vec<Declaracion>, String> {
    let mut lexer = forja_syntax::lexer::Lexer::nuevo(codigo);
    let tokens = lexer.tokenize().map_err(|errores| errores[0].to_string())?;
    let mut parser = forja_syntax::parser::Parser::nuevo(tokens);
    parser
        .parsear()
        .map(|programa| programa.declaraciones)
        .map_err(|errores| errores[0].to_string())
}

// ═══════════════════════════════════════════════════════════════════
// TIPOS DE VALOR
// ═══════════════════════════════════════════════════════════════════

#[derive(Debug, Clone, PartialEq)]
pub enum ValorGUI {
    Texto(String),
    Entero(i64),
    Decimal(f64),
    Booleano(bool),
    Nulo,
}

impl ValorGUI {
    fn es_verdadero(&self) -> bool {
        match self {
            ValorGUI::Booleano(b) => *b,
            ValorGUI::Entero(n) => *n != 0,
            ValorGUI::Decimal(n) => *n != 0.0,
            ValorGUI::Texto(t) => !t.is_empty(),
            ValorGUI::Nulo => false,
        }
    }

    fn to_display(&self) -> String {
        match self {
            ValorGUI::Texto(s) => s.clone(),
            ValorGUI::Entero(n) => n.to_string(),
            ValorGUI::Decimal(f) => {
                if *f == f.trunc() {
                    format!("{:.1}", f)
                } else {
                    f.to_string()
                }
            }
            ValorGUI::Booleano(b) => if *b { "verdadero" } else { "falso" }.to_string(),
            ValorGUI::Nulo => "nulo".to_string(),
        }
    }

    fn to_json_valor(&self) -> serde_json::Value {
        match self {
            ValorGUI::Texto(t) => serde_json::Value::String(t.clone()),
            ValorGUI::Entero(n) => serde_json::Value::Number((*n).into()),
            ValorGUI::Decimal(f) => serde_json::Number::from_f64(*f)
                .map(serde_json::Value::Number)
                .unwrap_or(serde_json::Value::Null),
            ValorGUI::Booleano(b) => serde_json::Value::Bool(*b),
            ValorGUI::Nulo => serde_json::Value::Null,
        }
    }

    fn from_serde(val: &serde_json::Value) -> Self {
        match val {
            serde_json::Value::String(s) => ValorGUI::Texto(s.clone()),
            serde_json::Value::Number(n) => n
                .as_i64()
                .map(ValorGUI::Entero)
                .or_else(|| n.as_f64().map(ValorGUI::Decimal))
                .unwrap_or(ValorGUI::Nulo),
            serde_json::Value::Bool(b) => ValorGUI::Booleano(*b),
            _ => ValorGUI::Nulo,
        }
    }

    fn to_f64(&self) -> f64 {
        match self {
            ValorGUI::Entero(n) => *n as f64,
            ValorGUI::Decimal(f) => *f,
            ValorGUI::Texto(s) => s.parse().unwrap_or(0.0),
            _ => 0.0,
        }
    }
}

// ═══════════════════════════════════════════════════════════════════
// LAYOUT (representación intermedia de UI)
// ═══════════════════════════════════════════════════════════════════

/// Tono de una insignia (el color lo pone el CSS).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TonoInsignia {
    Neutro,
    Exito,
    Aviso,
    Error,
}

/// Gravedad de un aviso: define el `role` accesible.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gravedad {
    Informacion,
    Advertencia,
    Error,
}

#[derive(Debug, Clone)]
pub enum Layout {
    Column {
        children: Vec<Layout>,
        gap: f64,
    },
    Row {
        children: Vec<Layout>,
        gap: f64,
    },
    PilaZ(Vec<Layout>),
    Label {
        texto: String,
    },
    VariableLabel {
        variable: String,
    },
    Button {
        texto: String,
        callback: String,
    },
    EntradaTexto {
        variable: String,
        placeholder: String,
    },
    Title(String),
    Spacer(f64),
    /// Pestañas: la activa sale de `variable` (acción `tab:<variable>:<valor>`).
    Pestanas {
        variable: String,
        items: Vec<(String, String, Vec<Layout>)>,
    },
    /// Lista de opciones con selección (acción `sel:<variable>:<valor>`).
    ListaSeleccion {
        variable: String,
        etiqueta: String,
        por_defecto: String,
        opciones: Vec<(String, String)>,
    },
    /// Bloque plegable nativo.
    Plegable {
        titulo: String,
        hijos: Vec<Layout>,
        abierto: bool,
    },
    /// Barra de progreso (accesible).
    Progreso {
        valor: f64,
        maximo: f64,
        etiqueta: String,
    },
    /// Indicador de carga.
    Cargando {
        texto: String,
    },
    /// Botón sólo icono, con texto accesible en `aria-label`.
    BotonIcono {
        icono: String,
        etiqueta: String,
        accion: String,
    },
    /// Formulario: Enter envía; `accion` vacía lo maneja el runtime.
    Formulario {
        accion: String,
        metodo: String,
        hijos: Vec<Layout>,
    },
    /// Texto plano (celdas de tabla): se escapa, sin envolver en `<p>`.
    Texto(String),
    /// Tabla: cabeceras + filas de celdas. `ordenable` hace que las cabeceras
    /// sean botones (`orden:<columna>`) y que el runtime ordene las filas.
    Tabla {
        cabeceras: Vec<String>,
        filas: Vec<Vec<Layout>>,
        ordenable: bool,
    },
    /// Insignia/chip de estado.
    Insignia {
        texto: String,
        tono: TonoInsignia,
    },
    /// Aviso con gravedad (define el `role`).
    Aviso {
        texto: String,
        gravedad: Gravedad,
    },
    /// Diálogo nativo (`abrir:<id>` / `cerrar:<id>`).
    Dialogo {
        id: String,
        titulo: String,
        hijos: Vec<Layout>,
        abierto: bool,
    },
    /// Función de GUI que no se reconoce (se avisa en la interfaz)
    Unimplemented(String),
}

// ═══════════════════════════════════════════════════════════════════
// VARIABLE STORE (simplificado, single-threaded para WASM)
// ═══════════════════════════════════════════════════════════════════

#[derive(Debug, Clone)]
pub struct VariableAlmacen {
    variables: Rc<RefCell<HashMap<String, serde_json::Value>>>,
}

impl VariableAlmacen {
    pub fn nuevo() -> Self {
        VariableAlmacen {
            variables: Rc::new(RefCell::new(HashMap::new())),
        }
    }

    pub fn obtener(&self, name: &str) -> Option<serde_json::Value> {
        self.variables.borrow().get(name).cloned()
    }

    pub fn fijar(&self, name: &str, value: serde_json::Value) {
        self.variables.borrow_mut().insert(name.to_string(), value);
    }

    pub fn contiene(&self, name: &str) -> bool {
        self.variables.borrow().contains_key(name)
    }

    /// Nombres de todas las variables guardadas.
    pub fn nombres(&self) -> Vec<String> {
        self.variables.borrow().keys().cloned().collect()
    }

    /// Todas las variables como (nombre, texto), para sembrar el `Estado` de
    /// bindings. Copiar todo es más simple y robusto que adivinar desde el árbol
    /// qué variables existen.
    pub fn como_textos(&self) -> Vec<(String, String)> {
        self.variables
            .borrow()
            .iter()
            .map(|(nombre, valor)| {
                let texto = match valor {
                    serde_json::Value::String(s) => s.clone(),
                    otro => otro.to_string(),
                };
                (nombre.clone(), texto)
            })
            .collect()
    }
}

impl Default for VariableAlmacen {
    fn default() -> Self {
        Self::nuevo()
    }
}

// ═══════════════════════════════════════════════════════════════════
// ÁMBITO DE VARIABLES (para evaluador)
// ═══════════════════════════════════════════════════════════════════

struct Ambito {
    variables: HashMap<String, ValorGUI>,
}

impl Ambito {
    fn nuevo() -> Self {
        Ambito {
            variables: HashMap::new(),
        }
    }

    fn obtener(&self, nombre: &str) -> Option<&ValorGUI> {
        self.variables.get(nombre)
    }

    fn asignar(&mut self, nombre: String, valor: ValorGUI) {
        self.variables.insert(nombre, valor);
    }
}

// ═══════════════════════════════════════════════════════════════════
// COLORES DEL TEMA (Material You simplificado)
// ═══════════════════════════════════════════════════════════════════

#[allow(dead_code)]
// ═══════════════════════════════════════════════════════════════════
// AST → LAYOUT (conversión simplificada)
// ═══════════════════════════════════════════════════════════════════

/// Extrae el layout del AST buscando la función `main` y convirtiendo
/// las llamadas a funciones de UI al Layout correspondiente.
fn extraer_layout(decls: &[Declaracion]) -> Layout {
    for decl in decls {
        if let Declaracion::Funcion { nombre, cuerpo, .. } = decl {
            if nombre == "main" {
                for d in cuerpo {
                    if let Declaracion::LlamadaFuncion { nombre, argumentos } = d {
                        let expr = Expresion::LlamadaFuncion {
                            nombre: nombre.clone(),
                            argumentos: argumentos.clone(),
                        };
                        if let Some(layout) = expr_a_layout(&expr) {
                            return layout;
                        }
                    } else if let Declaracion::Expresion(expr) = d {
                        if let Some(layout) = expr_a_layout(expr) {
                            return layout;
                        }
                    }
                }
            }
        }
    }
    // Si no hay función main o no se encontró layout, retornar columna vacía
    Layout::Column {
        children: vec![],
        gap: 0.0,
    }
}

/// Convierte una expresión del AST a un Layout
/// Un renglón de tabla: `renglon(celda1, celda2, ...)`.
fn expr_a_renglon(expr: &Expresion) -> Option<Vec<Layout>> {
    let Expresion::LlamadaFuncion { nombre, argumentos } = expr else {
        return None;
    };
    if nombre != "renglon" && nombre != "row" && nombre != "tr" {
        return None;
    }
    Some(
        argumentos
            .iter()
            .map(|argumento| match argumento {
                Expresion::LiteralTexto(texto) => Some(Layout::Texto(texto.clone())),
                otro => expr_a_layout(otro),
            })
            .collect::<Option<Vec<Layout>>>()
            .unwrap_or_default(),
    )
}

/// Una opción: `opcion("valor", "Etiqueta")`.
fn expr_a_opcion(expr: &Expresion) -> Option<(String, String)> {
    let Expresion::LlamadaFuncion { nombre, argumentos } = expr else {
        return None;
    };
    if nombre != "opcion" && nombre != "option" {
        return None;
    }
    let valor = match argumentos.first() {
        Some(Expresion::LiteralTexto(s)) => s.clone(),
        _ => return None,
    };
    let etiqueta = match argumentos.get(1) {
        Some(Expresion::LiteralTexto(s)) => s.clone(),
        _ => valor.clone(),
    };
    Some((valor, etiqueta))
}

/// Una pestaña: `pestana("valor", "Etiqueta", hijos...)`.
fn expr_a_pestana(expr: &Expresion) -> Option<(String, String, Vec<Layout>)> {
    let Expresion::LlamadaFuncion { nombre, argumentos } = expr else {
        return None;
    };
    if nombre != "pestana" && nombre != "tab" {
        return None;
    }
    let valor = match argumentos.first() {
        Some(Expresion::LiteralTexto(s)) => s.clone(),
        _ => return None,
    };
    let etiqueta = match argumentos.get(1) {
        Some(Expresion::LiteralTexto(s)) => s.clone(),
        _ => valor.clone(),
    };
    let hijos = argumentos
        .iter()
        .skip(2)
        .filter_map(expr_a_layout)
        .collect();
    Some((valor, etiqueta, hijos))
}

fn expr_a_layout(expr: &Expresion) -> Option<Layout> {
    match expr {
        Expresion::LlamadaFuncion { nombre, argumentos } => {
            match nombre.as_str() {
                // Layout containers
                "columna" | "gui_columna" | "Column" => {
                    let children = procesar_args(argumentos);
                    Some(Layout::Column { children, gap: 0.0 })
                }
                "fila" | "gui_fila" | "Row" => {
                    let children = procesar_args(argumentos);
                    Some(Layout::Row { children, gap: 0.0 })
                }
                "pila" | "gui_pila" | "zstack" | "ZStack" => {
                    let children = procesar_args(argumentos);
                    Some(Layout::PilaZ(children))
                }

                // Labels / Texto
                "escribir" | "etiqueta" | "label" | "text" | "Label" => {
                    if let Some(arg) = argumentos.first() {
                        match arg {
                            Expresion::Identificador { nombre: v, .. } => {
                                Some(Layout::VariableLabel {
                                    variable: v.clone(),
                                })
                            }
                            Expresion::LiteralTexto(s) => Some(Layout::Label { texto: s.clone() }),
                            _ => Some(Layout::Label {
                                texto: format!("{:?}", arg),
                            }),
                        }
                    } else {
                        Some(Layout::Spacer(0.0))
                    }
                }

                "etiqueta_titulo" | "titulo" | "title" | "Title" => {
                    let texto = argumentos
                        .first()
                        .map(|a| match a {
                            Expresion::LiteralTexto(s) => s.clone(),
                            _ => String::new(),
                        })
                        .unwrap_or_default();
                    Some(Layout::Title(texto))
                }

                "etiqueta_dinamica" | "varlabel" | "VariableLabel" => {
                    let variable = argumentos
                        .first()
                        .map(|a| match a {
                            Expresion::Identificador { nombre: s, .. } => s.clone(),
                            Expresion::LiteralTexto(s) => s.clone(),
                            _ => String::new(),
                        })
                        .unwrap_or_default();
                    Some(Layout::VariableLabel { variable })
                }

                // Buttons
                "boton" | "button" | "btn" | "Button" => {
                    let texto = extraer_texto(argumentos, 0);
                    let callback = extraer_callback(argumentos, 1);
                    Some(Layout::Button { texto, callback })
                }
                "boton_relleno" | "filled_button" | "FilledButton" => {
                    let texto = extraer_texto(argumentos, 0);
                    let callback = extraer_callback(argumentos, 1);
                    Some(Layout::Button { texto, callback })
                }

                // Text Input
                "entrada_texto" | "text_input" | "input" | "TextInput" => {
                    let variable = argumentos
                        .first()
                        .map(|a| match a {
                            Expresion::LiteralTexto(s) => s.clone(),
                            Expresion::Identificador { nombre: s, .. } => s.clone(),
                            _ => String::new(),
                        })
                        .unwrap_or_default();
                    let placeholder = argumentos
                        .get(1)
                        .map(|a| match a {
                            Expresion::LiteralTexto(s) => s.clone(),
                            _ => String::new(),
                        })
                        .unwrap_or_default();
                    Some(Layout::EntradaTexto {
                        variable,
                        placeholder,
                    })
                }

                // Spacer
                "pestanas" | "tabs" | "Tabs" => {
                    let variable = argumentos
                        .first()
                        .and_then(|a| match a {
                            Expresion::LiteralTexto(s) => Some(s.clone()),
                            _ => None,
                        })
                        .unwrap_or_default();
                    let mut items = Vec::new();
                    for argumento in argumentos.iter().skip(1) {
                        if let Some(item) = expr_a_pestana(argumento) {
                            items.push(item);
                        }
                    }
                    Some(Layout::Pestanas { variable, items })
                }
                "seleccion_lista" | "lista_seleccion" | "opciones" | "listbox" => {
                    let variable = match argumentos.first() {
                        Some(Expresion::LiteralTexto(s)) => s.clone(),
                        _ => String::new(),
                    };
                    let etiqueta = match argumentos.get(1) {
                        Some(Expresion::LiteralTexto(s)) => s.clone(),
                        _ => String::new(),
                    };
                    let por_defecto = match argumentos.get(2) {
                        Some(Expresion::LiteralTexto(s)) => s.clone(),
                        _ => String::new(),
                    };
                    let mut opciones = Vec::new();
                    for argumento in argumentos.iter().skip(3) {
                        if let Some(opcion) = expr_a_opcion(argumento) {
                            opciones.push(opcion);
                        }
                    }
                    Some(Layout::ListaSeleccion {
                        variable,
                        etiqueta,
                        por_defecto,
                        opciones,
                    })
                }
                "progreso" | "progress" | "Progreso" => {
                    let valor = match argumentos.first() {
                        Some(Expresion::LiteralNumero(n)) => *n as f64,
                        _ => 0.0,
                    };
                    let maximo = match argumentos.get(1) {
                        Some(Expresion::LiteralNumero(n)) => *n as f64,
                        _ => 100.0,
                    };
                    let etiqueta = match argumentos.get(2) {
                        Some(Expresion::LiteralTexto(s)) => s.clone(),
                        _ => String::new(),
                    };
                    Some(Layout::Progreso {
                        valor,
                        maximo,
                        etiqueta,
                    })
                }
                "cargando" | "spinner" | "Cargando" => {
                    let texto = match argumentos.first() {
                        Some(Expresion::LiteralTexto(s)) => s.clone(),
                        _ => "Cargando".to_string(),
                    };
                    Some(Layout::Cargando { texto })
                }
                "boton_icono" | "icon_button" | "IconButton" => {
                    let icono = extraer_texto(argumentos, 0);
                    let etiqueta = extraer_texto(argumentos, 1);
                    let accion = extraer_callback(argumentos, 2);
                    Some(Layout::BotonIcono {
                        icono,
                        etiqueta,
                        accion,
                    })
                }
                "formulario" | "form" | "Formulario" => {
                    let accion = match argumentos.first() {
                        Some(Expresion::LiteralTexto(s)) => s.clone(),
                        _ => String::new(),
                    };
                    let metodo = match argumentos.get(1) {
                        Some(Expresion::LiteralTexto(s)) => s.clone(),
                        _ => "post".to_string(),
                    };
                    let hijos = argumentos
                        .iter()
                        .skip(2)
                        .filter_map(expr_a_layout)
                        .collect();
                    Some(Layout::Formulario {
                        accion,
                        metodo,
                        hijos,
                    })
                }
                "tabla" | "table" | "Table" | "tabla_ordenable" | "table_sortable" => {
                    let ordenable = nombre == "tabla_ordenable" || nombre == "table_sortable";
                    let mut cabeceras: Vec<String> = Vec::new();
                    let mut filas: Vec<Vec<Layout>> = Vec::new();
                    for argumento in argumentos {
                        if let Some(celdas) = expr_a_renglon(argumento) {
                            filas.push(celdas);
                        } else if let Expresion::LiteralTexto(texto) = argumento {
                            cabeceras.push(texto.clone());
                        }
                    }
                    Some(Layout::Tabla {
                        cabeceras,
                        filas,
                        ordenable,
                    })
                }
                "insignia" | "chip" | "badge" | "Insignia" => {
                    let texto = match argumentos.first() {
                        Some(Expresion::LiteralTexto(s)) => s.clone(),
                        _ => String::new(),
                    };
                    let tono = match argumentos.get(1) {
                        Some(Expresion::LiteralTexto(t)) => match t.as_str() {
                            "exito" | "éxito" => TonoInsignia::Exito,
                            "aviso" => TonoInsignia::Aviso,
                            "error" => TonoInsignia::Error,
                            _ => TonoInsignia::Neutro,
                        },
                        _ => TonoInsignia::Neutro,
                    };
                    Some(Layout::Insignia { texto, tono })
                }
                "aviso" | "alerta" | "notice" | "Aviso" => {
                    let texto = match argumentos.first() {
                        Some(Expresion::LiteralTexto(s)) => s.clone(),
                        _ => String::new(),
                    };
                    let gravedad = match argumentos.get(1) {
                        Some(Expresion::LiteralTexto(g)) => match g.as_str() {
                            "advertencia" => Gravedad::Advertencia,
                            "error" => Gravedad::Error,
                            _ => Gravedad::Informacion,
                        },
                        _ => Gravedad::Informacion,
                    };
                    Some(Layout::Aviso { texto, gravedad })
                }
                "plegable" | "detalles" | "details" | "Plegable" => {
                    let titulo = match argumentos.first() {
                        Some(Expresion::LiteralTexto(s)) => s.clone(),
                        _ => String::new(),
                    };
                    let abierto =
                        matches!(argumentos.get(1), Some(Expresion::LiteralBooleano(true)));
                    let hijos = argumentos
                        .iter()
                        .skip(2)
                        .filter_map(expr_a_layout)
                        .collect();
                    Some(Layout::Plegable {
                        titulo,
                        hijos,
                        abierto,
                    })
                }
                "dialogo" | "dialog" | "Dialogo" => {
                    let id = match argumentos.first() {
                        Some(Expresion::LiteralTexto(s)) => s.clone(),
                        _ => String::new(),
                    };
                    let titulo = match argumentos.get(1) {
                        Some(Expresion::LiteralTexto(s)) => s.clone(),
                        _ => String::new(),
                    };
                    let abierto =
                        matches!(argumentos.get(2), Some(Expresion::LiteralBooleano(true)));
                    let hijos = argumentos
                        .iter()
                        .skip(3)
                        .filter_map(expr_a_layout)
                        .collect();
                    Some(Layout::Dialogo {
                        id,
                        titulo,
                        hijos,
                        abierto,
                    })
                }
                "espacio" | "spacer" | "Spacer" => {
                    let tam = argumentos
                        .first()
                        .and_then(|a| match a {
                            Expresion::LiteralNumero(n) => Some(*n as f64),
                            _ => None,
                        })
                        .unwrap_or(10.0);
                    Some(Layout::Spacer(tam))
                }

                // Si es una función desconocida, mostrar como unimplemented
                _ => {
                    let args_desc: Vec<String> = argumentos
                        .iter()
                        .map(|a| match a {
                            Expresion::LiteralTexto(s) => s.clone(),
                            Expresion::LiteralNumero(n) => n.to_string(),
                            _ => "?".to_string(),
                        })
                        .collect();
                    Some(Layout::Unimplemented(
                        format!("{}(", nombre) + &args_desc.join(", ") + ")",
                    ))
                }
            }
        }
        Expresion::Identificador { nombre, .. } => Some(Layout::VariableLabel {
            variable: nombre.clone(),
        }),
        Expresion::LiteralTexto(s) => Some(Layout::Label { texto: s.clone() }),
        _ => None,
    }
}

fn procesar_args(args: &[Expresion]) -> Vec<Layout> {
    args.iter().filter_map(expr_a_layout).collect()
}

fn extraer_texto(args: &[Expresion], index: usize) -> String {
    args.get(index)
        .map(|a| match a {
            Expresion::LiteralTexto(s) => s.clone(),
            _ => String::new(),
        })
        .unwrap_or_default()
}

fn extraer_callback(args: &[Expresion], index: usize) -> String {
    args.get(index)
        .map(|a| match a {
            Expresion::Referencia { expr, .. } => match expr.as_ref() {
                Expresion::Identificador { nombre: n, .. } => n.clone(),
                _ => String::new(),
            },
            Expresion::Identificador { nombre: n, .. } => n.clone(),
            // Un texto como acción permite usar convenciones del runtime
            // (`"abrir:confirmar"`), no sólo funciones Forja.
            Expresion::LiteralTexto(s) => s.clone(),
            _ => String::new(),
        })
        .unwrap_or_default()
}

#[allow(dead_code)]
fn extraer_booleano(args: &[Expresion], index: usize) -> bool {
    args.get(index)
        .and_then(|a| match a {
            Expresion::LiteralBooleano(b) => Some(*b),
            _ => None,
        })
        .unwrap_or(false)
}

// ═══════════════════════════════════════════════════════════════════
// EVALUADOR SIMPLIFICADO (tree-walking)
// ═══════════════════════════════════════════════════════════════════

/// Busca una declaración de función por nombre
fn buscar_funcion<'a>(
    nombre: &str,
    declaraciones: &'a [Declaracion],
) -> Result<&'a Declaracion, String> {
    for d in declaraciones {
        if let Declaracion::Funcion { nombre: ref n, .. } = d {
            if n == nombre {
                return Ok(d);
            }
        }
    }
    Err(format!("Función '{}' no encontrada", nombre))
}

/// Evalúa una función Forja a partir del AST
fn ejecutar_funcion(
    nombre: &str,
    args: &[ValorGUI],
    declaraciones: &[Declaracion],
    almacen: &VariableAlmacen,
) -> Result<ValorGUI, String> {
    buscar_funcion(nombre, declaraciones).and_then(|func| {
        let (parametros, cuerpo) = match func {
            Declaracion::Funcion {
                parametros, cuerpo, ..
            } => (parametros, cuerpo),
            _ => return Err(format!("'{}' no es una función", nombre)),
        };
        let mut ambito = Ambito::nuevo();

        for (i, param) in parametros.iter().enumerate() {
            if let Some(val) = args.get(i) {
                ambito.asignar(param.nombre.clone(), val.clone());
            } else if let Some(json_val) = almacen.obtener(&param.nombre) {
                ambito.asignar(param.nombre.clone(), ValorGUI::from_serde(&json_val));
            }
        }

        evaluar_bloque(cuerpo, &mut ambito, almacen, declaraciones)
    })
}

/// Evalúa un bloque de declaraciones
fn evaluar_bloque(
    bloque: &[Declaracion],
    ambito: &mut Ambito,
    almacen: &VariableAlmacen,
    declaraciones: &[Declaracion],
) -> Result<ValorGUI, String> {
    for declaracion in bloque {
        let resultado = evaluar_declaracion(declaracion, ambito, almacen, declaraciones)?;
        if es_retorno(declaracion) {
            return Ok(resultado);
        }
    }
    Ok(ValorGUI::Nulo)
}

fn es_retorno(decl: &Declaracion) -> bool {
    matches!(decl, Declaracion::Retornar { .. })
}

fn evaluar_declaracion(
    decl: &Declaracion,
    ambito: &mut Ambito,
    almacen: &VariableAlmacen,
    declaraciones: &[Declaracion],
) -> Result<ValorGUI, String> {
    match decl {
        Declaracion::Retornar { valor } => {
            if let Some(expr) = valor {
                evaluar_expresion(expr, ambito, almacen, declaraciones)
            } else {
                Ok(ValorGUI::Nulo)
            }
        }

        Declaracion::Si {
            condicion,
            bloque_verdadero,
            bloque_falso,
        } => {
            let cond_val = evaluar_expresion(condicion, ambito, almacen, declaraciones)?;
            if cond_val.es_verdadero() {
                evaluar_bloque(bloque_verdadero, ambito, almacen, declaraciones)
            } else if let Some(sino_bloque) = bloque_falso {
                evaluar_bloque(sino_bloque, ambito, almacen, declaraciones)
            } else {
                Ok(ValorGUI::Nulo)
            }
        }

        Declaracion::Mientras { condicion, bloque } => {
            loop {
                let cond_val = evaluar_expresion(condicion, ambito, almacen, declaraciones)?;
                if !cond_val.es_verdadero() {
                    break;
                }
                let resultado = evaluar_bloque(bloque, ambito, almacen, declaraciones)?;
                if !matches!(resultado, ValorGUI::Nulo) {
                    return Ok(resultado);
                }
            }
            Ok(ValorGUI::Nulo)
        }

        Declaracion::Variable { nombre, valor, .. } => {
            let val = if let Some(expr) = valor {
                evaluar_expresion(expr, ambito, almacen, declaraciones)?
            } else {
                ValorGUI::Nulo
            };
            ambito.asignar(nombre.clone(), val.clone());
            almacen.fijar(nombre, val.to_json_valor());
            Ok(ValorGUI::Nulo)
        }

        Declaracion::Asignacion { nombre, valor, .. } => {
            let val = evaluar_expresion(valor, ambito, almacen, declaraciones)?;
            ambito.asignar(nombre.clone(), val.clone());
            almacen.fijar(nombre, val.to_json_valor());
            Ok(ValorGUI::Nulo)
        }

        Declaracion::LlamadaFuncion { nombre, argumentos } => {
            let mut args = Vec::new();
            for arg in argumentos {
                args.push(evaluar_expresion(arg, ambito, almacen, declaraciones)?);
            }
            ejecutar_funcion(nombre, &args, declaraciones, almacen)
        }

        Declaracion::Expresion(expr) => {
            evaluar_expresion(expr, ambito, almacen, declaraciones)?;
            Ok(ValorGUI::Nulo)
        }

        Declaracion::Para {
            inicializacion,
            condicion,
            incremento,
            bloque,
        } => {
            if let Some(init) = inicializacion {
                evaluar_declaracion(init, ambito, almacen, declaraciones)?;
            }
            loop {
                if let Some(cond) = condicion {
                    let cond_val = evaluar_expresion(cond, ambito, almacen, declaraciones)?;
                    if !cond_val.es_verdadero() {
                        break;
                    }
                }
                let resultado = evaluar_bloque(bloque, ambito, almacen, declaraciones)?;
                if !matches!(resultado, ValorGUI::Nulo) {
                    return Ok(resultado);
                }
                if let Some(inc) = incremento {
                    evaluar_declaracion(inc, ambito, almacen, declaraciones)?;
                }
            }
            Ok(ValorGUI::Nulo)
        }

        Declaracion::Cuando {
            condicion, cuerpo, ..
        } => {
            let cond_val = evaluar_expresion(condicion, ambito, almacen, declaraciones)?;
            if cond_val.es_verdadero() {
                evaluar_bloque(cuerpo, ambito, almacen, declaraciones)
            } else {
                Ok(ValorGUI::Nulo)
            }
        }

        // Tipos, funciones, clases y otras declaraciones que se ignoran en runtime
        Declaracion::Funcion { .. }
        | Declaracion::Clase { .. }
        | Declaracion::Importar(_)
        | Declaracion::ImportarExterna(_)
        | Declaracion::Enum { .. }
        | Declaracion::Rasgo { .. }
        | Declaracion::Implementacion { .. }
        | Declaracion::AccesoMiembro { .. }
        | Declaracion::AsignacionMiembro { .. }
        | Declaracion::AsignacionIndex { .. }
        | Declaracion::AsignacionMultiple { .. }
        | Declaracion::Repetir { .. }
        | Declaracion::Romper
        | Declaracion::Continuar => Ok(ValorGUI::Nulo),
    }
}

fn evaluar_expresion(
    expr: &Expresion,
    ambito: &mut Ambito,
    almacen: &VariableAlmacen,
    declaraciones: &[Declaracion],
) -> Result<ValorGUI, String> {
    match expr {
        Expresion::LiteralNumero(n) => Ok(ValorGUI::Entero(*n)),
        Expresion::LiteralDecimal(f) => Ok(ValorGUI::Decimal(*f)),
        Expresion::LiteralTexto(s) => Ok(ValorGUI::Texto(s.clone())),
        Expresion::LiteralBooleano(b) => Ok(ValorGUI::Booleano(*b)),
        Expresion::LiteralNulo => Ok(ValorGUI::Nulo),

        // `si` como elemento de layout: corre la rama elegida, sin valor.
        Expresion::Si {
            condicion,
            bloque_verdadero,
            bloque_falso,
        } => {
            let cond = evaluar_expresion(condicion, ambito, almacen, declaraciones)?;
            let rama = if cond.es_verdadero() {
                Some(bloque_verdadero)
            } else {
                bloque_falso.as_ref()
            };
            match rama {
                Some(bloque) => evaluar_bloque(bloque, ambito, almacen, declaraciones),
                None => Ok(ValorGUI::Nulo),
            }
        }

        Expresion::Identificador { nombre, .. } => {
            if let Some(val) = ambito.obtener(nombre) {
                Ok(val.clone())
            } else if let Some(json_val) = almacen.obtener(nombre) {
                Ok(ValorGUI::from_serde(&json_val))
            } else {
                Err(format!("Variable '{}' no encontrada", nombre))
            }
        }

        Expresion::Binaria {
            izquierda,
            operador,
            derecha,
        } => {
            let izq = evaluar_expresion(izquierda, ambito, almacen, declaraciones)?;
            let der = evaluar_expresion(derecha, ambito, almacen, declaraciones)?;
            evaluar_binaria(izq, operador, der)
        }

        Expresion::Unaria {
            operador,
            expr: inner,
        } => {
            let val = evaluar_expresion(inner, ambito, almacen, declaraciones)?;
            match operador {
                OperadorUnario::Negar => match val {
                    ValorGUI::Entero(n) => Ok(ValorGUI::Entero(-n)),
                    ValorGUI::Decimal(f) => Ok(ValorGUI::Decimal(-f)),
                    _ => Err("No se puede negar un valor no numérico".to_string()),
                },
                OperadorUnario::No => Ok(ValorGUI::Booleano(!val.es_verdadero())),
            }
        }

        Expresion::LlamadaFuncion { nombre, argumentos } => {
            let mut args = Vec::new();
            for arg in argumentos {
                args.push(evaluar_expresion(arg, ambito, almacen, declaraciones)?);
            }
            ejecutar_funcion(nombre, &args, declaraciones, almacen)
        }

        Expresion::AccesoMiembro { objeto, miembro } => {
            let obj = evaluar_expresion(objeto, ambito, almacen, declaraciones)?;
            let clave = format!("{}_{}", obj.to_display(), miembro);
            if let Some(json_val) = almacen.obtener(&clave) {
                Ok(ValorGUI::from_serde(&json_val))
            } else {
                Ok(ValorGUI::Nulo)
            }
        }

        Expresion::Grupo(inner) => evaluar_expresion(inner, ambito, almacen, declaraciones),

        Expresion::Asignacion { variable, valor } => {
            let val = evaluar_expresion(valor, ambito, almacen, declaraciones)?;
            ambito.asignar(variable.clone(), val.clone());
            almacen.fijar(variable, val.to_json_valor());
            Ok(val)
        }

        // `x += 1` y compañía: se evalúa el valor actual, se aplica el
        // operador y se vuelve a asignar. En la GUI sólo se soportan
        // identificadores simples (miembro/índice queda como error claro).
        Expresion::AsignacionCompuesta {
            objetivo,
            operador,
            valor,
        } => {
            if let Expresion::Identificador { nombre, .. } = objetivo.as_ref() {
                let actual = evaluar_expresion(objetivo, ambito, almacen, declaraciones)?;
                let incremento = evaluar_expresion(valor, ambito, almacen, declaraciones)?;
                let nuevo = evaluar_binaria(actual, operador, incremento)?;
                ambito.asignar(nombre.clone(), nuevo.clone());
                almacen.fijar(nombre, nuevo.to_json_valor());
                Ok(nuevo)
            } else {
                Err(
                    "Asignación compuesta sobre miembro o índice no soportada en la GUI"
                        .to_string(),
                )
            }
        }

        Expresion::Arreglo(elementos) => {
            let mut values = Vec::new();
            for elem in elementos {
                values.push(evaluar_expresion(elem, ambito, almacen, declaraciones)?);
            }
            let json_arr: Vec<serde_json::Value> =
                values.iter().map(|v| v.to_json_valor()).collect();
            let json_str = serde_json::to_string(&json_arr)
                .map_err(|e| format!("Error serializando array: {}", e))?;
            Ok(ValorGUI::Texto(json_str))
        }

        Expresion::Mapa(pares) => {
            let mut mapa = serde_json::Map::new();
            for (k, v) in pares {
                let clave_val = evaluar_expresion(k, ambito, almacen, declaraciones)?;
                let val = evaluar_expresion(v, ambito, almacen, declaraciones)?;
                mapa.insert(clave_val.to_display(), val.to_json_valor());
            }
            let json_str = serde_json::to_string(&serde_json::Value::Object(mapa))
                .map_err(|e| format!("Error serializando mapa: {}", e))?;
            Ok(ValorGUI::Texto(json_str))
        }

        Expresion::Ok(inner) => evaluar_expresion(inner, ambito, almacen, declaraciones),

        Expresion::Error(inner) => {
            let val = evaluar_expresion(inner, ambito, almacen, declaraciones)?;
            Err(format!("Error: {}", val.to_display()))
        }

        Expresion::Algo(inner) => evaluar_expresion(inner, ambito, almacen, declaraciones),

        Expresion::Referencia { expr: inner, .. } => {
            evaluar_expresion(inner, ambito, almacen, declaraciones)
        }

        Expresion::Index { objeto, indice } => {
            let obj = evaluar_expresion(objeto, ambito, almacen, declaraciones)?;
            let idx = evaluar_expresion(indice, ambito, almacen, declaraciones)?;
            let idx_num = match idx {
                ValorGUI::Entero(n) => n as usize,
                _ => return Err("Índice debe ser un entero".to_string()),
            };
            if let Ok(serde_json::Value::Array(arr)) =
                serde_json::from_str::<serde_json::Value>(&obj.to_display())
            {
                if idx_num < arr.len() {
                    Ok(ValorGUI::from_serde(&arr[idx_num]))
                } else {
                    Err(format!(
                        "Índice {} fuera de rango (len={})",
                        idx_num,
                        arr.len()
                    ))
                }
            } else {
                Err("No se puede indexar un valor que no es un array".to_string())
            }
        }

        // No implementados en runtime
        Expresion::Closure { .. } => Err("Closures no soportados en WASM GUI".to_string()),
        Expresion::Hilo { .. } => Err("Hilos no soportados en WASM GUI".to_string()),
        Expresion::CanalNuevo(_) => Err("Canales no soportados en WASM GUI".to_string()),
        Expresion::Seleccionar { .. } => Err("Seleccionar no soportado en WASM GUI".to_string()),
        Expresion::Try(inner) => {
            let val = evaluar_expresion(inner, ambito, almacen, declaraciones)?;
            if matches!(val, ValorGUI::Nulo) {
                Err("Error propagado?".to_string())
            } else {
                Ok(val)
            }
        }
        Expresion::Resultado => Ok(ValorGUI::Nulo),
        Expresion::Anterior(inner) => evaluar_expresion(inner, ambito, almacen, declaraciones),

        // No implementados
        Expresion::Instanciacion { .. } => {
            Err("Instanciación no soportada en WASM GUI".to_string())
        }
        Expresion::LiteralExacto(_, _) => Err("Literal exacto no soportado".to_string()),
        Expresion::AsignacionCampo { .. } => Err("Asignación de campo no soportada".to_string()),
        Expresion::ArregloFijar { .. } => Err("ArraySet no soportado".to_string()),
        Expresion::Coincidir { .. } => Err("Match no soportado en WASM GUI".to_string()),
        Expresion::Ternario {
            condicion,
            si_verdadero,
            si_falso,
        } => {
            let cond = evaluar_expresion(condicion, ambito, almacen, declaraciones)?;
            let b = match &cond {
                ValorGUI::Booleano(b) => *b,
                ValorGUI::Entero(n) => *n != 0,
                _ => false,
            };
            if b {
                evaluar_expresion(si_verdadero, ambito, almacen, declaraciones)
            } else {
                evaluar_expresion(si_falso, ambito, almacen, declaraciones)
            }
        }
        Expresion::LlamadaMetodo { .. } => Ok(ValorGUI::Nulo),
        Expresion::Nada | Expresion::Ninguno => Ok(ValorGUI::Nulo),
    }
}

fn evaluar_binaria(izq: ValorGUI, operador: &Operador, der: ValorGUI) -> Result<ValorGUI, String> {
    match operador {
        Operador::Suma => Ok(sumar(izq, der)),
        Operador::Resta => Ok(restar(izq, der)),
        Operador::Multiplicacion => Ok(multiplicar(izq, der)),
        Operador::Division => Ok(dividir(izq, der)),
        Operador::Modulo => match (izq, der) {
            (ValorGUI::Entero(a), ValorGUI::Entero(b)) => {
                if b == 0 {
                    Err("División por cero en módulo".to_string())
                } else {
                    Ok(ValorGUI::Entero(a % b))
                }
            }
            _ => Err("Módulo sólo soportado para enteros".to_string()),
        },
        Operador::IgualIgual => Ok(ValorGUI::Booleano(izq == der)),
        Operador::Diferente => Ok(ValorGUI::Booleano(izq != der)),
        Operador::Menor | Operador::MenorIgual | Operador::Mayor | Operador::MayorIgual => {
            let a = izq.to_f64();
            let b = der.to_f64();
            Ok(ValorGUI::Booleano(match operador {
                Operador::Menor => a < b,
                Operador::MenorIgual => a <= b,
                Operador::Mayor => a > b,
                Operador::MayorIgual => a >= b,
                _ => false,
            }))
        }
        Operador::Y => Ok(ValorGUI::Booleano(izq.es_verdadero() && der.es_verdadero())),
        Operador::O => Ok(ValorGUI::Booleano(izq.es_verdadero() || der.es_verdadero())),
    }
}

fn sumar(a: ValorGUI, b: ValorGUI) -> ValorGUI {
    match (a, b) {
        (ValorGUI::Entero(a), ValorGUI::Entero(b)) => ValorGUI::Entero(a + b),
        (ValorGUI::Decimal(a), ValorGUI::Decimal(b)) => ValorGUI::Decimal(a + b),
        (ValorGUI::Entero(a), ValorGUI::Decimal(b)) => ValorGUI::Decimal(a as f64 + b),
        (ValorGUI::Decimal(a), ValorGUI::Entero(b)) => ValorGUI::Decimal(a + b as f64),
        (ValorGUI::Texto(a), ValorGUI::Texto(b)) => ValorGUI::Texto(a + &b),
        (ValorGUI::Texto(a), b) => ValorGUI::Texto(a + &b.to_display()),
        (a, ValorGUI::Texto(b)) => ValorGUI::Texto(a.to_display() + &b),
        _ => ValorGUI::Nulo,
    }
}

fn restar(a: ValorGUI, b: ValorGUI) -> ValorGUI {
    match (a, b) {
        (ValorGUI::Entero(a), ValorGUI::Entero(b)) => ValorGUI::Entero(a - b),
        (ValorGUI::Decimal(a), ValorGUI::Decimal(b)) => ValorGUI::Decimal(a - b),
        (ValorGUI::Entero(a), ValorGUI::Decimal(b)) => ValorGUI::Decimal(a as f64 - b),
        (ValorGUI::Decimal(a), ValorGUI::Entero(b)) => ValorGUI::Decimal(a - b as f64),
        _ => ValorGUI::Nulo,
    }
}

fn multiplicar(a: ValorGUI, b: ValorGUI) -> ValorGUI {
    match (a, b) {
        (ValorGUI::Entero(a), ValorGUI::Entero(b)) => ValorGUI::Entero(a * b),
        (ValorGUI::Decimal(a), ValorGUI::Decimal(b)) => ValorGUI::Decimal(a * b),
        (ValorGUI::Entero(a), ValorGUI::Decimal(b)) => ValorGUI::Decimal(a as f64 * b),
        (ValorGUI::Decimal(a), ValorGUI::Entero(b)) => ValorGUI::Decimal(a * b as f64),
        _ => ValorGUI::Nulo,
    }
}

fn dividir(a: ValorGUI, b: ValorGUI) -> ValorGUI {
    match (a, b) {
        (ValorGUI::Entero(a), ValorGUI::Entero(b)) => {
            if b == 0 {
                ValorGUI::Nulo
            } else {
                ValorGUI::Entero(a / b)
            }
        }
        (ValorGUI::Decimal(a), ValorGUI::Decimal(b)) => {
            if b == 0.0 {
                ValorGUI::Nulo
            } else {
                ValorGUI::Decimal(a / b)
            }
        }
        (ValorGUI::Entero(a), ValorGUI::Decimal(b)) => {
            if b == 0.0 {
                ValorGUI::Nulo
            } else {
                ValorGUI::Decimal(a as f64 / b)
            }
        }
        (ValorGUI::Decimal(a), ValorGUI::Entero(b)) => {
            if b == 0 {
                ValorGUI::Nulo
            } else {
                ValorGUI::Decimal(a / b as f64)
            }
        }
        _ => ValorGUI::Nulo,
    }
}

// ═══════════════════════════════════════════════════════════════════
// INICIALIZACIÓN DE ESTADO
// ═══════════════════════════════════════════════════════════════════

/// Inicializa el estado evaluando la función `main` (variables iniciales)
fn valor_variable(
    expr: &Expresion,
    ambito: &mut Ambito,
    almacen: &VariableAlmacen,
    declaraciones: &[Declaracion],
) -> ValorGUI {
    match expr {
        Expresion::LiteralNumero(n) => ValorGUI::Entero(*n),
        Expresion::LiteralDecimal(f) => ValorGUI::Decimal(*f),
        Expresion::LiteralTexto(s) => ValorGUI::Texto(s.clone()),
        Expresion::LiteralBooleano(b) => ValorGUI::Booleano(*b),
        Expresion::LiteralNulo => ValorGUI::Nulo,
        _ => evaluar_expresion(expr, ambito, almacen, declaraciones).unwrap_or(ValorGUI::Nulo),
    }
}

fn inicializar_estado(declaraciones: &[Declaracion], almacen: &VariableAlmacen) {
    let mut ambito = Ambito::nuevo();

    // Variables globales (top-level): los callbacks las ven a través del store.
    for decl in declaraciones {
        if let Declaracion::Variable { nombre, valor, .. } = decl {
            let val = match valor {
                Some(expr) => valor_variable(expr, &mut ambito, almacen, declaraciones),
                None => ValorGUI::Nulo,
            };
            ambito.asignar(nombre.clone(), val.clone());
            almacen.fijar(nombre, val.to_json_valor());
        }
    }

    // Variables locales de main().
    for decl in declaraciones {
        if let Declaracion::Funcion { nombre, cuerpo, .. } = decl {
            if nombre == "main" {
                for d in cuerpo {
                    if let Declaracion::Variable { nombre, valor, .. } = d {
                        let val = match valor {
                            Some(expr) => valor_variable(expr, &mut ambito, almacen, declaraciones),
                            None => ValorGUI::Nulo,
                        };
                        ambito.asignar(nombre.clone(), val.clone());
                        almacen.fijar(nombre, val.to_json_valor());
                    }
                }
                return;
            }
        }
    }
}

// ═══════════════════════════════════════════════════════════════════
// EVENTOS (click handling)
// ═══════════════════════════════════════════════════════════════════

// ═══════════════════════════════════════════════════════════════════
// WASM EXPORTS
// ═══════════════════════════════════════════════════════════════════

/// Retorna la versión del crate
#[wasm_bindgen]
pub fn version() -> String {
    format!("forja-wasm-gui v{}", env!("CARGO_PKG_VERSION"))
}

/// Ejecuta código Forja y lo pinta como **DOM** en `raiz_id`.
///
/// Nombre que usa el playground de la documentación (antes pintaba un canvas).
#[wasm_bindgen]
pub fn ejecutar_dom(raiz_id: &str, codigo: &str) -> String {
    renderizar_dom(raiz_id, codigo)
}

/// Repinta el DOM con el estado actual, sin recompilar el programa.
#[wasm_bindgen]
pub fn rerenderizar_dom() -> String {
    repintar_dom();
    "ok".to_string()
}

// ═══════════════════════════════════════════════════════════════════
// RENDERIZADO DOM (reemplaza al canvas, Fase 4 del plan web)
// ═══════════════════════════════════════════════════════════════════
//
// El canvas no da accesibilidad, ni formularios nativos, ni CSS, ni SEO. Acá el
// mismo `Layout` se convierte en nodos DOM reales (`forja_gui_dom::Nodo`) y se
// serializa a HTML: el navegador hace el layout, el foco, el scroll y el teclado.

use forja_gui_dom::{Nodo as NodoDom, TipoEntrada, VarianteBoton};

/// Traduce el árbol de layout de la GUI a nodos DOM (puro, sin browser).
fn layout_a_nodos(layout: &Layout) -> Vec<NodoDom> {
    match layout {
        Layout::Column { children, .. } => vec![NodoDom::Contenedor {
            hijos: children.iter().flat_map(layout_a_nodos).collect(),
            clase: "fg-columna".to_string(),
        }],
        Layout::Row { children, .. } => vec![NodoDom::Contenedor {
            hijos: children.iter().flat_map(layout_a_nodos).collect(),
            clase: "fg-fila".to_string(),
        }],
        Layout::PilaZ(hijos) => vec![NodoDom::Contenedor {
            hijos: hijos.iter().flat_map(layout_a_nodos).collect(),
            clase: "fg-pila".to_string(),
        }],
        Layout::Label { texto } => vec![NodoDom::Parrafo(texto.clone())],
        Layout::VariableLabel { variable } => vec![NodoDom::Variable(variable.clone())],
        Layout::Button { texto, callback } => vec![NodoDom::Boton {
            texto: texto.clone(),
            accion: if callback.is_empty() {
                format!("click:{}", texto.to_lowercase())
            } else {
                callback.clone()
            },
            variante: VarianteBoton::Primario,
            deshabilitado: false,
        }],
        Layout::EntradaTexto {
            variable,
            placeholder,
        } => vec![NodoDom::Entrada {
            nombre: variable.clone(),
            tipo: TipoEntrada::Texto,
            valor: String::new(),
            marcador: placeholder.clone(),
            requerido: false,
            error: None,
        }],
        Layout::Title(texto) => vec![NodoDom::Encabezado {
            nivel: 1,
            texto: texto.clone(),
        }],
        Layout::Spacer(_) => vec![NodoDom::Contenedor {
            hijos: Vec::new(),
            clase: "fg-espacio".to_string(),
        }],
        Layout::Pestanas { variable, items } => vec![NodoDom::Pestanas {
            variable: variable.clone(),
            items: items
                .iter()
                .map(|(valor, etiqueta, hijos)| {
                    (
                        valor.clone(),
                        etiqueta.clone(),
                        hijos.iter().flat_map(layout_a_nodos).collect(),
                    )
                })
                .collect(),
        }],
        Layout::Progreso {
            valor,
            maximo,
            etiqueta,
        } => vec![NodoDom::Progreso {
            valor: *valor,
            maximo: *maximo,
            etiqueta: etiqueta.clone(),
        }],
        Layout::Cargando { texto } => vec![NodoDom::Cargando {
            texto: texto.clone(),
        }],
        Layout::BotonIcono {
            icono,
            etiqueta,
            accion,
        } => vec![NodoDom::BotonIcono {
            icono: icono.clone(),
            etiqueta: etiqueta.clone(),
            accion: accion.clone(),
            deshabilitado: false,
        }],
        Layout::Formulario {
            accion,
            metodo,
            hijos,
        } => vec![NodoDom::Formulario {
            accion: accion.clone(),
            metodo: metodo.clone(),
            hijos: hijos.iter().flat_map(layout_a_nodos).collect(),
        }],
        Layout::Texto(texto) => vec![NodoDom::Texto(texto.clone())],
        Layout::Tabla {
            cabeceras,
            filas,
            ordenable,
        } => vec![NodoDom::Tabla {
            cabeceras: cabeceras.clone(),
            filas: filas
                .iter()
                .map(|fila| fila.iter().flat_map(layout_a_nodos).collect())
                .collect(),
            orden: if *ordenable {
                cabeceras
                    .first()
                    .map(|primera| (primera.clone(), forja_gui_dom::Orden::Ascendente))
            } else {
                None
            },
        }],
        Layout::Insignia { texto, tono } => vec![NodoDom::Insignia {
            texto: texto.clone(),
            tono: match tono {
                TonoInsignia::Neutro => forja_gui_dom::Tono::Neutro,
                TonoInsignia::Exito => forja_gui_dom::Tono::Exito,
                TonoInsignia::Aviso => forja_gui_dom::Tono::Aviso,
                TonoInsignia::Error => forja_gui_dom::Tono::Error,
            },
        }],
        Layout::Aviso { texto, gravedad } => vec![NodoDom::Aviso {
            texto: texto.clone(),
            nivel: match gravedad {
                Gravedad::Informacion => forja_gui_dom::NivelAviso::Informacion,
                Gravedad::Advertencia => forja_gui_dom::NivelAviso::Advertencia,
                Gravedad::Error => forja_gui_dom::NivelAviso::Error,
            },
        }],
        Layout::ListaSeleccion {
            variable,
            etiqueta,
            por_defecto,
            opciones,
        } => vec![NodoDom::ListaSeleccion {
            variable: variable.clone(),
            etiqueta: etiqueta.clone(),
            por_defecto: por_defecto.clone(),
            opciones: opciones.clone(),
        }],
        Layout::Plegable {
            titulo,
            hijos,
            abierto,
        } => vec![NodoDom::Plegable {
            titulo: titulo.clone(),
            hijos: hijos.iter().flat_map(layout_a_nodos).collect(),
            abierto: *abierto,
        }],
        Layout::Dialogo {
            id,
            titulo,
            hijos,
            abierto,
        } => vec![NodoDom::Dialogo {
            id: id.clone(),
            titulo: titulo.clone(),
            hijos: hijos.iter().flat_map(layout_a_nodos).collect(),
            abierto: *abierto,
        }],
        Layout::Unimplemented(nombre) => vec![NodoDom::Contenedor {
            hijos: vec![NodoDom::Texto(format!("[no implementado: {nombre}]"))],
            clase: "fg-no-implementado".to_string(),
        }],
    }
}

thread_local! {
    /// Estado del modo DOM: árbol compilado, variables y estado de bindings.
    static DOM_STATE: RefCell<OpcionesDom> = RefCell::new(OpcionesDom::nuevo());
}

struct OpcionesDom {
    declaraciones: Vec<Declaracion>,
    almacen: VariableAlmacen,
    estado: forja_gui_dom::Estado,
    raiz: String,
    /// Los listeners van a la raíz (que no se recrea) y se enganchan una sola
    /// vez: si se re-engancharan en cada repintado, un clic dispararía N veces.
    enganchado: bool,
}

impl OpcionesDom {
    fn nuevo() -> Self {
        OpcionesDom {
            declaraciones: Vec::new(),
            almacen: VariableAlmacen::nuevo(),
            estado: forja_gui_dom::Estado::nuevo(),
            raiz: String::new(),
            enganchado: false,
        }
    }
}

/// Copia al `Estado` de bindings los valores que el programa tiene en su
/// almacén. Es lo que hace que un callback que cambia una variable se vea.
fn sincronizar_estado() {
    DOM_STATE.with(|celda| {
        let mut estado_dom = celda.borrow_mut();
        for (nombre, texto) in estado_dom.almacen.como_textos() {
            estado_dom.estado.fijar(&nombre, texto);
        }
    });
}

/// Vuelve a pintar el árbol con el estado actual y re-engancha los listeners.
/// Actualiza el DOM existente con el estado actual, **sin recrear el árbol**.
///
/// `set_inner_html` de todo el árbol fuerza parseo de HTML, layout y paint
/// completos (30-60 ms con esta pantalla) y genera basura que aparece como
/// picos de GC. Acá se tocan sólo los nodos que dependen del estado.
fn actualizar_dom() {
    let (raiz_id, estado) = DOM_STATE.with(|celda| {
        let estado_dom = celda.borrow();
        (estado_dom.raiz.clone(), estado_dom.estado.clone())
    });
    let Some(documento) = web_sys::window().and_then(|w| w.document()) else {
        return;
    };
    let Some(raiz) = documento.get_element_by_id(&raiz_id) else {
        return;
    };

    // 1. Variables ligadas: inputs, textareas, selects y textos.
    if let Ok(nodos) = raiz.query_selector_all("[data-fg-variable]") {
        for indice in 0..nodos.length() {
            let Some(nodo) = nodos.get(indice) else {
                continue;
            };
            let Ok(elemento) = nodo.dyn_into::<web_sys::Element>() else {
                continue;
            };
            let Some(nombre) = elemento.get_attribute("data-fg-variable") else {
                continue;
            };
            let etiqueta = elemento.tag_name().to_lowercase();
            match etiqueta.as_str() {
                "input" => {
                    let Ok(entrada) = elemento.dyn_into::<web_sys::HtmlInputElement>() else {
                        continue;
                    };
                    if entrada.type_() == "checkbox" {
                        entrada.set_checked(estado.booleano(&nombre));
                    } else {
                        entrada.set_value(estado.obtener(&nombre).unwrap_or(""));
                    }
                }
                "select" => {
                    if let Ok(seleccion) = elemento.dyn_into::<web_sys::HtmlSelectElement>() {
                        seleccion.set_value(estado.obtener(&nombre).unwrap_or(""));
                    }
                }
                "textarea" => {
                    if let Ok(area) = elemento.dyn_into::<web_sys::HtmlTextAreaElement>() {
                        area.set_value(estado.obtener(&nombre).unwrap_or(""));
                    }
                }
                _ => {
                    elemento.set_text_content(estado.obtener(&nombre));
                }
            }
        }
    }

    // 2. Pestañas y paneles: `aria-selected` y `hidden`.
    if let Ok(pestanas) = raiz.query_selector_all("[role=tab]") {
        for indice in 0..pestanas.length() {
            let Some(nodo) = pestanas.get(indice) else {
                continue;
            };
            let Ok(pestana) = nodo.dyn_into::<web_sys::Element>() else {
                continue;
            };
            let Some(accion) = pestana.get_attribute("data-fg-accion") else {
                continue;
            };
            let Some(resto) = accion.strip_prefix("tab:") else {
                continue;
            };
            let Some((variable, valor)) = resto.split_once(':') else {
                continue;
            };
            let activa = estado.obtener(variable).unwrap_or("");
            let _ = pestana.set_attribute(
                "aria-selected",
                if activa == valor { "true" } else { "false" },
            );
        }
    }
    if let Ok(paneles) = raiz.query_selector_all("[data-fg-panel]") {
        for indice in 0..paneles.length() {
            let Some(nodo) = paneles.get(indice) else {
                continue;
            };
            let Ok(panel) = nodo.dyn_into::<web_sys::Element>() else {
                continue;
            };
            let (Some(valor), Some(variable)) = (
                panel.get_attribute("data-fg-panel"),
                panel.get_attribute("data-fg-panel-var"),
            ) else {
                continue;
            };
            let activo = estado.obtener(&variable).unwrap_or("");
            if activo == valor {
                let _ = panel.remove_attribute("hidden");
            } else {
                let _ = panel.set_attribute("hidden", "");
            }
        }
    }

    // 3. Listas con selección: `aria-selected` por opción.
    if let Ok(opciones) = raiz.query_selector_all("[role=option]") {
        for indice in 0..opciones.length() {
            let Some(nodo) = opciones.get(indice) else {
                continue;
            };
            let Ok(opcion) = nodo.dyn_into::<web_sys::Element>() else {
                continue;
            };
            let Some(accion) = opcion.get_attribute("data-fg-accion") else {
                continue;
            };
            let Some(resto) = accion.strip_prefix("sel:") else {
                continue;
            };
            let Some((variable, valor)) = resto.split_once(':') else {
                continue;
            };
            let elegida = estado.obtener(variable).unwrap_or("");
            let _ = opcion.set_attribute(
                "aria-selected",
                if elegida == valor { "true" } else { "false" },
            );
        }
    }
}

fn repintar_dom() {
    let (html, raiz) = DOM_STATE.with(|celda| {
        let estado_dom = celda.borrow();
        let layout = extraer_layout(&estado_dom.declaraciones);
        let nodos = layout_a_nodos(&layout);
        let html = nodos
            .iter()
            .map(|nodo| forja_gui_dom::render_con_estado(nodo, &estado_dom.estado))
            .collect::<String>();
        (html.clone(), estado_dom.raiz.clone())
    });

    let Some(document) = web_sys::window().and_then(|w| w.document()) else {
        return;
    };
    let Some(raiz) = document.get_element_by_id(&raiz) else {
        return;
    };
    raiz.set_inner_html(&html);

    let ya_enganchado = DOM_STATE.with(|celda| celda.borrow().enganchado);
    if !ya_enganchado {
        enganchar_dom();
        DOM_STATE.with(|celda| celda.borrow_mut().enganchado = true);
    }
}

/// Engancha un único listener delegado por tipo de evento sobre la raíz.
///
/// Delegar evita crear un closure por widget (y por render); el widget se
/// identifica leyendo `data-fg-accion` / `data-fg-variable` del `target`.
fn enganchar_dom() {
    use wasm_bindgen::closure::Closure;

    let Some(document) = web_sys::window().and_then(|w| w.document()) else {
        return;
    };
    let raiz = DOM_STATE.with(|celda| celda.borrow().raiz.clone());
    let Some(elemento) = document.get_element_by_id(&raiz) else {
        return;
    };

    // Clic: ejecuta la función Forja de la acción y repinta.
    let al_hacer_clic = Closure::<dyn FnMut(web_sys::Event)>::new(move |evento: web_sys::Event| {
        let Some(accion) = atributo_del_evento(&evento, forja_gui_dom::eventos::ACCION) else {
            return;
        };
        // Convenciones de interfaz que no necesitan una función Forja:
        //   tab:<variable>:<valor>  → cambia la pestaña activa
        //   abrir:<id> / cerrar:<id> → diálogo nativo
        // Navegación por URL: el hash es el historial (atrás/adelante funcionan).
        if let Some(ruta_hash) = accion.strip_prefix("ir:") {
            if let Some(ventana) = web_sys::window() {
                if let Ok(historial) = ventana.history() {
                    let _ = historial.push_state_with_url(&JsValue::NULL, "", Some(ruta_hash));
                    // `push_state` no dispara `popstate`: se aplica a mano.
                    aplicar_ruta(ruta_hash);
                }
            }
            return;
        }
        if let Some(columna) = accion.strip_prefix("orden:") {
            if let Some(target) = evento.target() {
                if let Ok(elemento) = target.dyn_into::<web_sys::Element>() {
                    ordenar_tabla(&elemento, columna);
                }
            }
            return;
        }
        if let Some(resto) = accion
            .strip_prefix("tab:")
            .or_else(|| accion.strip_prefix("sel:"))
        {
            if let Some((variable, valor)) = resto.split_once(':') {
                DOM_STATE.with(|celda| {
                    let mut estado_dom = celda.borrow_mut();
                    estado_dom.estado.fijar(variable, valor);
                    estado_dom
                        .almacen
                        .fijar(variable, serde_json::Value::String(valor.to_string()));
                });
                actualizar_dom();
            }
            return;
        }
        if let Some(id) = accion.strip_prefix("abrir:") {
            if let Some(dialogo) = dialogo_por_id(id) {
                let _ = dialogo.show_modal();
            }
            return;
        }
        if let Some(id) = accion.strip_prefix("cerrar:") {
            if let Some(dialogo) = dialogo_por_id(id) {
                let _ = dialogo.close();
            }
            return;
        }

        let (declaraciones, almacen) = DOM_STATE.with(|celda| {
            let estado_dom = celda.borrow();
            (estado_dom.declaraciones.clone(), estado_dom.almacen.clone())
        });
        match ejecutar_funcion(&accion, &[], &declaraciones, &almacen) {
            Ok(_) => {
                // El callback pudo cambiar variables: se reflejan antes de pintar.
                sincronizar_estado();
                actualizar_dom()
            }
            Err(e) => web_sys::console::log_1(&format!("acción '{}' falló: {}", accion, e).into()),
        }
    });
    let _ =
        elemento.add_event_listener_with_callback("click", al_hacer_clic.as_ref().unchecked_ref());
    al_hacer_clic.forget();

    // Cambio de valor: actualiza el estado y el almacén, y repinta.
    let al_cambiar = Closure::<dyn FnMut(web_sys::Event)>::new(move |evento: web_sys::Event| {
        let Some(variable) = atributo_del_evento(&evento, "data-fg-variable") else {
            return;
        };
        let Some(target) = evento.target() else {
            return;
        };
        let Ok(entrada) = target.dyn_into::<web_sys::HtmlInputElement>() else {
            return;
        };
        let valor = if entrada.type_() == "checkbox" {
            let marcado = entrada.checked();
            DOM_STATE.with(|celda| celda.borrow_mut().estado.fijar_booleano(&variable, marcado));
            if marcado { "verdadero" } else { "falso" }.to_string()
        } else {
            let texto = entrada.value();
            DOM_STATE.with(|celda| celda.borrow_mut().estado.fijar(&variable, texto.clone()));
            texto
        };
        DOM_STATE.with(|celda| {
            celda
                .borrow()
                .almacen
                .fijar(&variable, serde_json::Value::String(valor));
        });
        // El campo cambió una variable: se actualizan los nodos ligados.
        actualizar_dom();
    });
    let _ =
        elemento.add_event_listener_with_callback("change", al_cambiar.as_ref().unchecked_ref());
    al_cambiar.forget();

    // Teclado en las pestañas: el patrón ARIA pide flechas, Home y End.
    let al_teclear =
        Closure::<dyn FnMut(web_sys::KeyboardEvent)>::new(move |evento: web_sys::KeyboardEvent| {
            let tecla = evento.key();
            if !matches!(tecla.as_str(), "ArrowRight" | "ArrowLeft" | "Home" | "End") {
                return;
            }
            let Some(target) = evento.target() else {
                return;
            };
            let Ok(target) = target.dyn_into::<web_sys::Element>() else {
                return;
            };
            if target.get_attribute("role").as_deref() != Some("tab") {
                return;
            }
            let raiz = DOM_STATE.with(|celda| celda.borrow().raiz.clone());
            let Some(documento) = web_sys::window().and_then(|w| w.document()) else {
                return;
            };
            let Some(contenedor) = documento.get_element_by_id(&raiz) else {
                return;
            };
            let Ok(pestanas) = contenedor.query_selector_all("[role=\"tab\"]") else {
                return;
            };
            let total = pestanas.length() as i64;
            if total == 0 {
                return;
            }

            let mut actual: Option<i64> = None;
            for indice in 0..total {
                if let Some(nodo) = pestanas.get(indice as u32) {
                    let es_el_mismo = nodo
                        .dyn_ref::<web_sys::Node>()
                        .map(|n| n.is_same_node(Some(target.unchecked_ref())))
                        .unwrap_or(false);
                    if es_el_mismo {
                        actual = Some(indice);
                        break;
                    }
                }
            }
            let Some(actual) = actual else { return };

            let destino = match tecla.as_str() {
                "ArrowRight" => (actual + 1) % total,
                "ArrowLeft" => (actual + total - 1) % total,
                "Home" => 0,
                _ => total - 1,
            };

            let Some(nodo_destino) = pestanas.get(destino as u32) else {
                return;
            };
            let Ok(elemento_destino) = nodo_destino.dyn_into::<web_sys::Element>() else {
                return;
            };
            let Some(accion) = elemento_destino.get_attribute("data-fg-accion") else {
                return;
            };
            let Some(resto) = accion.strip_prefix("tab:") else {
                return;
            };
            let Some((variable, valor)) = resto.split_once(':') else {
                return;
            };

            DOM_STATE.with(|celda| {
                let mut estado_dom = celda.borrow_mut();
                estado_dom.estado.fijar(variable, valor);
                estado_dom
                    .almacen
                    .fijar(variable, serde_json::Value::String(valor.to_string()));
            });
            actualizar_dom();

            // Se devuelve el foco a la pestaña nueva (el árbol no se recrea, pero
            // el botón no recibe foco solo con cambiar aria-selected).
            if let Some(nuevo) = documento.get_element_by_id(&raiz) {
                if let Ok(Some(boton)) =
                    nuevo.query_selector(&format!("[data-fg-accion=\"{accion}\"]"))
                {
                    if let Some(boton) = boton.dyn_ref::<web_sys::HtmlElement>() {
                        boton.focus().ok();
                    }
                }
            }
        });
    let _ =
        elemento.add_event_listener_with_callback("keydown", al_teclear.as_ref().unchecked_ref());
    al_teclear.forget();

    // Envío del formulario (Enter en un campo): se evita la navegación y se
    // dispara la acción del primer botón, que es lo que haría el usuario.
    let al_enviar = Closure::<dyn FnMut(web_sys::Event)>::new(move |evento: web_sys::Event| {
        evento.prevent_default();
        let Some(target) = evento.target() else {
            return;
        };
        let Ok(formulario) = target.dyn_into::<web_sys::Element>() else {
            return;
        };
        let Ok(Some(boton)) = formulario.query_selector("[data-fg-accion]") else {
            return;
        };
        if let Some(boton) = boton.dyn_ref::<web_sys::HtmlElement>() {
            boton.click();
        }
    });
    let _ = elemento.add_event_listener_with_callback("submit", al_enviar.as_ref().unchecked_ref());
    al_enviar.forget();

    // Atrás/adelante del navegador: se reaplica la ruta.
    let al_volver = Closure::<dyn FnMut(web_sys::Event)>::new(move |_evento: web_sys::Event| {
        aplicar_ruta(&ruta_actual());
    });
    if let Some(ventana) = web_sys::window() {
        let _ = ventana
            .add_event_listener_with_callback("popstate", al_volver.as_ref().unchecked_ref());
    }
    al_volver.forget();
}

/// Aplica una ruta (`#/demo/detalle`) al estado y repinta.
///
/// La variable `ruta` queda disponible para el programa, así una pantalla puede
/// decidir qué mostrar según la URL.
fn aplicar_ruta(ruta: &str) {
    let limpia = ruta.trim_start_matches('#').to_string();
    DOM_STATE.with(|celda| {
        let mut estado_dom = celda.borrow_mut();
        estado_dom.estado.fijar("ruta", limpia.clone());
        estado_dom
            .almacen
            .fijar("ruta", serde_json::Value::String(limpia));
    });
    actualizar_dom();
}

/// Ruta actual del navegador (con el `#` inicial).
fn ruta_actual() -> String {
    web_sys::window()
        .and_then(|w| w.location().hash().ok())
        .unwrap_or_default()
}

/// Ordena la tabla del botón clickeado por la columna de su cabecera.
///
/// No necesita lógica de la app: compara el texto de las celdas del `tbody` y
/// alterna ascendente/descendente, dejando `aria-sort` en las cabeceras.
fn ordenar_tabla(boton: &web_sys::Element, columna: &str) {
    let Some(tabla) = boton.closest("table").ok().flatten() else {
        return;
    };
    let Ok(cabeceras) = tabla.query_selector_all("thead th") else {
        return;
    };
    let mut indice: Option<u32> = None;
    let mut ascendente = true;
    for i in 0..cabeceras.length() {
        let Some(nodo) = cabeceras.get(i) else {
            continue;
        };
        let Ok(th) = nodo.dyn_into::<web_sys::Element>() else {
            continue;
        };
        let es_la_columna = th
            .text_content()
            .map(|t| t.trim() == columna)
            .unwrap_or(false);
        if es_la_columna {
            indice = Some(i);
            ascendente = th.get_attribute("aria-sort").as_deref() != Some("ascending");
            let _ = th.set_attribute(
                "aria-sort",
                if ascendente {
                    "ascending"
                } else {
                    "descending"
                },
            );
        } else {
            let _ = th.set_attribute("aria-sort", "none");
        }
    }
    let Some(indice) = indice else { return };

    let Ok(cuerpo) = tabla.query_selector("tbody") else {
        return;
    };
    let Some(cuerpo) = cuerpo else { return };
    let Ok(filas) = cuerpo.query_selector_all("tr") else {
        return;
    };

    let mut datos: Vec<(String, web_sys::Element)> = Vec::new();
    for i in 0..filas.length() {
        let Some(nodo) = filas.get(i) else { continue };
        let Ok(fila) = nodo.dyn_into::<web_sys::Element>() else {
            continue;
        };
        let texto = fila
            .query_selector_all("td")
            .ok()
            .and_then(|celdas| celdas.get(indice))
            .and_then(|celda| celda.text_content())
            .unwrap_or_default()
            .trim()
            .to_lowercase();
        datos.push((texto, fila));
    }
    datos.sort_by(|a, b| {
        if ascendente {
            a.0.cmp(&b.0)
        } else {
            b.0.cmp(&a.0)
        }
    });

    // Reinsertar en orden: `append_child` mueve los nodos existentes.
    for (_, fila) in datos {
        let _ = cuerpo.append_child(&fila);
    }
}

/// Busca un `<dialog id="fg-dialogo-<id>">`.
fn dialogo_por_id(id: &str) -> Option<web_sys::HtmlDialogElement> {
    let document = web_sys::window()?.document()?;
    let elemento = document.get_element_by_id(&format!("fg-dialogo-{id}"))?;
    elemento.dyn_into::<web_sys::HtmlDialogElement>().ok()
}

/// Lee un atributo `data-fg-*` del `target` o de su ancestro más cercano.
fn atributo_del_evento(evento: &web_sys::Event, nombre: &str) -> Option<String> {
    let mut actual = evento.target()?.dyn_into::<web_sys::Element>().ok();
    while let Some(elemento) = actual {
        if let Some(valor) = elemento.get_attribute(nombre) {
            if !valor.is_empty() {
                return Some(valor);
            }
        }
        actual = elemento.parent_element();
    }
    None
}

/// Renderiza la interfaz como **DOM real** dentro del elemento `raiz_id`.
///
/// Es el reemplazo del canvas: los widgets quedan como `div`/`button`/`input`
/// con etiquetas accesibles, y los clics ejecutan la función Forja indicada en
/// la acción del botón. Devuelve "ok" o un mensaje de error.
#[wasm_bindgen]
pub fn renderizar_dom(raiz_id: &str, codigo: &str) -> String {
    let Some(document) = web_sys::window().and_then(|w| w.document()) else {
        return "Error: no document".to_string();
    };
    if document.get_element_by_id(raiz_id).is_none() {
        return format!("Error: no se encontró el elemento '{}'", raiz_id);
    }

    // `analizar` (lexer+parser+tipos+préstamos) alcanza para extraer el árbol:
    // no hace falta optimizar ni generar código Rust, y en wasm eso se nota.
    let declaraciones = match compilar_a_ast(codigo) {
        Ok(declaraciones) => declaraciones,
        Err(error) => return format!("Error de compilación: {error}"),
    };

    let almacen = VariableAlmacen::nuevo();
    inicializar_estado(&declaraciones, &almacen);

    DOM_STATE.with(|celda| {
        let mut estado_dom = celda.borrow_mut();
        estado_dom.almacen = almacen.clone();
        estado_dom.declaraciones = declaraciones.clone();
        estado_dom.estado = forja_gui_dom::Estado::nuevo();
        estado_dom.raiz = raiz_id.to_string();

        // La ruta actual es una variable más (`ruta`), para que la app decida.
        let inicial = ruta_actual().trim_start_matches('#').to_string();
        estado_dom.estado.fijar("ruta", inicial.clone());
        estado_dom
            .almacen
            .fijar("ruta", serde_json::Value::String(inicial));

        // Semilla del estado: todas las variables que el programa ya definió.
        for (nombre, texto) in almacen.como_textos() {
            estado_dom.estado.fijar(&nombre, texto);
        }
    });

    repintar_dom();
    "ok".to_string()
}

/// Diagnóstico del modo DOM (para depurar en el navegador).
#[wasm_bindgen]
pub fn diagnostico_dom() -> String {
    DOM_STATE.with(|celda| {
        let estado_dom = celda.borrow();
        let estado: Vec<String> = estado_dom
            .estado
            .valores()
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect();
        let almacen: Vec<String> = estado_dom
            .almacen
            .como_textos()
            .into_iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect();
        format!(
            "estado[{}] almacen[{}]",
            estado.join(","),
            almacen.join(",")
        )
    })
}

/// Fuerza un repintado completo (recrea el árbol). Sirve para comparar el costo
/// del repintado total contra la actualización dirigida.
#[wasm_bindgen]
pub fn repintar_completo() {
    repintar_dom();
}
