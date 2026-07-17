#![allow(dead_code)]
// Forja WASM GUI Runtime
// Renderiza interfaces Forja en un <canvas> HTML5 usando Canvas 2D API.
// No depende de winit, Xilem, Masonry ni Vello.
//
// Arquitectura:
//   - Compila código Forja a AST vía forja::compilar_con_ast()
//   - Convierte AST → Layout simplificado
//   - Renderiza Layout en Canvas 2D
//   - Maneja eventos (click) con hit-testing sobre rectángulos de widgets
//   - Ejecuta callbacks Forja mediante evaluador tree-walking simplificado

use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::{CanvasRenderingContext2d, HtmlCanvasElement};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use forja::ast::{Declaracion, Expresion, Operador, OperadorUnario};

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
            ValorGUI::Booleano(b) => {
                if *b { "verdadero" } else { "falso" }.to_string()
            }
            ValorGUI::Nulo => "nulo".to_string(),
        }
    }

    fn to_json_value(&self) -> serde_json::Value {
        match self {
            ValorGUI::Texto(t) => serde_json::Value::String(t.clone()),
            ValorGUI::Entero(n) => serde_json::Value::Number((*n).into()),
            ValorGUI::Decimal(f) => {
                serde_json::Number::from_f64(*f)
                    .map(serde_json::Value::Number)
                    .unwrap_or(serde_json::Value::Null)
            }
            ValorGUI::Booleano(b) => serde_json::Value::Bool(*b),
            ValorGUI::Nulo => serde_json::Value::Null,
        }
    }

    fn from_serde(val: &serde_json::Value) -> Self {
        match val {
            serde_json::Value::String(s) => ValorGUI::Texto(s.clone()),
            serde_json::Value::Number(n) => {
                n.as_i64()
                    .map(ValorGUI::Entero)
                    .or_else(|| n.as_f64().map(ValorGUI::Decimal))
                    .unwrap_or(ValorGUI::Nulo)
            }
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

#[derive(Debug, Clone)]
pub enum Layout {
    Column { children: Vec<Layout>, gap: f64 },
    Row { children: Vec<Layout>, gap: f64 },
    ZStack(Vec<Layout>),
    Label { texto: String },
    VariableLabel { variable: String },
    Button { texto: String, callback: String },
    TextInput { variable: String, placeholder: String },
    Title(String),
    Spacer(f64),
    /// Placeholder para layouts no implementados en Canvas
    Unimplemented(String),
}

// ═══════════════════════════════════════════════════════════════════
// VARIABLE STORE (simplificado, single-threaded para WASM)
// ═══════════════════════════════════════════════════════════════════

#[derive(Debug, Clone)]
pub struct VariableStore {
    variables: Rc<RefCell<HashMap<String, serde_json::Value>>>,
}

impl VariableStore {
    pub fn new() -> Self {
        VariableStore {
            variables: Rc::new(RefCell::new(HashMap::new())),
        }
    }

    pub fn get(&self, name: &str) -> Option<serde_json::Value> {
        self.variables.borrow().get(name).cloned()
    }

    pub fn set(&self, name: &str, value: serde_json::Value) {
        self.variables.borrow_mut().insert(name.to_string(), value);
    }

    pub fn contains(&self, name: &str) -> bool {
        self.variables.borrow().contains_key(name)
    }
}

impl Default for VariableStore {
    fn default() -> Self {
        Self::new()
    }
}

// ═══════════════════════════════════════════════════════════════════
// ÁMBITO DE VARIABLES (para evaluador)
// ═══════════════════════════════════════════════════════════════════

struct Ambito {
    variables: HashMap<String, ValorGUI>,
}

impl Ambito {
    fn new() -> Self {
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
// WIDGET RECT (para hit testing en eventos)
// ═══════════════════════════════════════════════════════════════════

#[derive(Debug, Clone)]
struct WidgetHitArea {
    x: f64,
    y: f64,
    ancho: f64,
    alto: f64,
    /// Referencia al Layout original para identificar el widget
    layout: Layout,
}

// ═══════════════════════════════════════════════════════════════════
// ESTADO GLOBAL DE LA GUI
// ═══════════════════════════════════════════════════════════════════

struct AppState {
    store: VariableStore,
    declaraciones: Vec<forja::ast::Declaracion>,
    hit_areas: Vec<WidgetHitArea>,
    ultimo_layout: Option<Layout>,
    canvas_ancho: f64,
    canvas_alto: f64,
}

impl AppState {
    fn new() -> Self {
        AppState {
            store: VariableStore::new(),
            declaraciones: Vec::new(),
            hit_areas: Vec::new(),
            ultimo_layout: None,
            canvas_ancho: 600.0,
            canvas_alto: 400.0,
        }
    }
}

thread_local! {
    static APP_STATE: RefCell<AppState> = RefCell::new(AppState::new());
}

// ═══════════════════════════════════════════════════════════════════
// COLORES DEL TEMA (Material You simplificado)
// ═══════════════════════════════════════════════════════════════════

#[allow(dead_code)]
const COLOR_PRIMARY: &str = "#6750A4";
const COLOR_ON_PRIMARY: &str = "#FFFFFF";
const COLOR_PRIMARY_CONTAINER: &str = "#EADDFF";
const COLOR_SECONDARY: &str = "#625B71";
const COLOR_ON_SECONDARY: &str = "#FFFFFF";
const COLOR_SURFACE: &str = "#F5F0F7";
const COLOR_ON_SURFACE: &str = "#1D1B20";
const COLOR_SURFACE_VARIANT: &str = "#E7E0EC";
const COLOR_ON_SURFACE_VARIANT: &str = "#49454F";
const COLOR_OUTLINE: &str = "#79747E";
const COLOR_BACKGROUND: &str = "#FEF7FF";
const COLOR_ERROR: &str = "#B3261E";
const COLOR_ON_ERROR: &str = "#FFFFFF";

// ═══════════════════════════════════════════════════════════════════
// RENDERIZADO CANVAS 2D
// ═══════════════════════════════════════════════════════════════════

/// Renderiza un layout completo en el canvas, retornando (ancho_usado, alto_usado)
fn renderizar_layout(
    ctx: &CanvasRenderingContext2d,
    layout: &Layout,
    x: f64,
    y: f64,
    ancho_disponible: f64,
    hit_areas: &mut Vec<WidgetHitArea>,
    store: &VariableStore,
) -> (f64, f64) {
    match layout {
        Layout::Column { children, gap } => {
            let mut cy: f64 = y + gap;
            let mut max_ancho: f64 = 0.0;
            for child in children {
                let usado = renderizar_layout(ctx, child, x + gap, cy, ancho_disponible - gap * 2.0, hit_areas, store);
                let alto: f64 = usado.1;
                max_ancho = max_ancho.max(usado.0 + gap * 2.0);
                cy += alto + gap;
            }
            (max_ancho.max(ancho_disponible), cy - y)
        }

        Layout::Row { children, gap } => {
            let mut cx: f64 = x + gap;
            let mut max_alto: f64 = 0.0;
            // Primera pasada: medir todos los hijos
            let mut medidas: Vec<(f64, f64)> = Vec::new();
            for child in children {
                let ancho_est = estimar_ancho(child, ancho_disponible / children.len() as f64);
                medidas.push((ancho_est, 40.0)); // alto estimado
            }
            for (i, child) in children.iter().enumerate() {
                let ancho_hijo = medidas[i].0;
                let usado = renderizar_layout(ctx, child, cx, y + gap, ancho_hijo, hit_areas, store);
                max_alto = max_alto.max(usado.1 + gap * 2.0);
                cx += ancho_hijo + gap;
            }
            (cx - x, max_alto.max(40.0))
        }

        Layout::ZStack(children) => {
            let mut max_ancho: f64 = 0.0;
            let mut max_alto: f64 = 0.0;
            for child in children {
                let usado = renderizar_layout(ctx, child, x, y, ancho_disponible, hit_areas, store);
                max_ancho = max_ancho.max(usado.0);
                max_alto = max_alto.max(usado.1);
            }
            (max_ancho, max_alto)
        }

        Layout::Label { texto } => {
            renderizar_label(ctx, texto, x, y, ancho_disponible)
        }

        Layout::VariableLabel { variable } => {
            let texto = store.get(variable)
                .map(|v| ValorGUI::from_serde(&v).to_display())
                .unwrap_or_else(|| format!("{{{{{}}}}}", variable));
            renderizar_label(ctx, &texto, x, y, ancho_disponible)
        }

        Layout::Button { texto, .. } => {
            renderizar_boton(ctx, texto, x, y, ancho_disponible, hit_areas, layout.clone())
        }

        Layout::TextInput { variable, placeholder } => {
            renderizar_text_input(ctx, variable, placeholder, x, y, ancho_disponible, store, hit_areas, layout.clone())
        }

        Layout::Title(texto) => {
            ctx.set_font("bold 20px sans-serif");
            ctx.set_fill_style_str(COLOR_ON_SURFACE);
            ctx.fill_text(texto, x + 4.0, y + 24.0).ok();
            (ancho_disponible.max(100.0), 32.0)
        }

        Layout::Spacer(tam) => {
            (ancho_disponible, *tam)
        }

        Layout::Unimplemented(desc) => {
            ctx.set_fill_style_str(COLOR_ERROR);
            ctx.set_font("12px sans-serif");
            ctx.fill_text(&format!("⚠ {}", desc), x + 4.0, y + 16.0).ok();
            (ancho_disponible, 24.0)
        }
    }
}

fn renderizar_label(
    ctx: &CanvasRenderingContext2d,
    texto: &str,
    x: f64,
    y: f64,
    ancho_disponible: f64,
) -> (f64, f64) {
    ctx.set_font("14px sans-serif");
    ctx.set_fill_style_str(COLOR_ON_SURFACE);
    let ancho_estimado = (texto.len() as f64 * 8.0 + 16.0).min(ancho_disponible);
    ctx.fill_text(texto, x + 4.0, y + 20.0).ok();
    (ancho_estimado.max(20.0), 28.0)
}

fn renderizar_boton(
    ctx: &CanvasRenderingContext2d,
    texto: &str,
    x: f64,
    y: f64,
    ancho_disponible: f64,
    hit_areas: &mut Vec<WidgetHitArea>,
    layout: Layout,
) -> (f64, f64) {
    let ancho = ancho_disponible.max(80.0).min(300.0);
    let alto = 38.0;
    let radio_esq = 20.0;

    // Sombra
    ctx.set_fill_style_str("rgba(0,0,0,0.15)");
    redondear_rect(ctx, x + 2.0, y + 2.0, ancho, alto, radio_esq);
    ctx.fill();

    // Fondo
    ctx.set_fill_style_str(COLOR_PRIMARY);
    redondear_rect(ctx, x, y, ancho, alto, radio_esq);
    ctx.fill();

    // Texto
    ctx.set_fill_style_str(COLOR_ON_PRIMARY);
    ctx.set_font("bold 14px sans-serif");
    let ancho_texto_est = (texto.len() as f64 * 9.0).min(ancho - 16.0);
    let tx = x + (ancho - ancho_texto_est) / 2.0;
    ctx.fill_text(texto, tx, y + 25.0).ok();

    // Registrar área de hit-testing
    hit_areas.push(WidgetHitArea {
        x, y, ancho, alto,
        layout,
    });

    (ancho, alto + 4.0)
}

fn renderizar_text_input(
    ctx: &CanvasRenderingContext2d,
    variable: &str,
    placeholder: &str,
    x: f64,
    y: f64,
    ancho_disponible: f64,
    store: &VariableStore,
    hit_areas: &mut Vec<WidgetHitArea>,
    layout: Layout,
) -> (f64, f64) {
    let ancho = ancho_disponible.max(120.0);
    let alto = 38.0;
    let radio_esq = 4.0;

    // Obtener valor actual
    let valor_actual = store.get(variable)
        .map(|v| ValorGUI::from_serde(&v).to_display())
        .unwrap_or_default();

    // Fondo
    ctx.set_fill_style_str(COLOR_SURFACE_VARIANT);
    redondear_rect(ctx, x, y, ancho, alto, radio_esq);
    ctx.fill();

    // Borde inferior (estilo filled)
    ctx.set_stroke_style_str(COLOR_OUTLINE);
    ctx.set_line_width(1.0);
    ctx.begin_path();
    ctx.move_to(x, y + alto);
    ctx.line_to(x + ancho, y + alto);
    ctx.stroke();

    // Texto del valor o placeholder
    if !valor_actual.is_empty() {
        ctx.set_fill_style_str(COLOR_ON_SURFACE);
        ctx.set_font("14px sans-serif");
        ctx.fill_text(&valor_actual, x + 8.0, y + 25.0).ok();
    } else {
        ctx.set_fill_style_str(COLOR_ON_SURFACE_VARIANT);
        ctx.set_font("14px sans-serif");
        ctx.fill_text(placeholder, x + 8.0, y + 25.0).ok();
    }

    // Registrar área de hit-testing
    hit_areas.push(WidgetHitArea {
        x, y, ancho, alto,
        layout,
    });

    (ancho, alto + 4.0)
}

/// Dibuja un rectángulo redondeado usando path 2D
fn redondear_rect(
    ctx: &CanvasRenderingContext2d,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    r: f64,
) {
    let radio = r.min(w / 2.0).min(h / 2.0);
    ctx.begin_path();
    ctx.move_to(x + radio, y);
    ctx.line_to(x + w - radio, y);
    ctx.arc(x + w - radio, y + radio, radio, -std::f64::consts::FRAC_PI_2, 0.0).ok();
    ctx.line_to(x + w, y + h - radio);
    ctx.arc(x + w - radio, y + h - radio, radio, 0.0, std::f64::consts::FRAC_PI_2).ok();
    ctx.line_to(x + radio, y + h);
    ctx.arc(x + radio, y + h - radio, radio, std::f64::consts::FRAC_PI_2, std::f64::consts::PI).ok();
    ctx.line_to(x, y + radio);
    ctx.arc(x + radio, y + radio, radio, std::f64::consts::PI, -std::f64::consts::FRAC_PI_2).ok();
    ctx.close_path();
}

/// Estima el ancho de un widget (para layout de filas)
fn estimar_ancho(layout: &Layout, _default_ancho: f64) -> f64 {
    match layout {
        Layout::Label { texto } | Layout::Title(texto) => {
            (texto.len() as f64 * 8.0 + 16.0).min(250.0)
        }
        Layout::VariableLabel { .. } => 100.0,
        Layout::Button { texto, .. } => {
            (texto.len() as f64 * 9.0 + 32.0).max(80.0).min(200.0)
        }
        Layout::TextInput { .. } => 150.0,
        Layout::Spacer(tam) => *tam,
        Layout::Column { .. } => _default_ancho,
        Layout::Row { .. } => _default_ancho,
        Layout::ZStack { .. } => _default_ancho,
        Layout::Unimplemented(_) => _default_ancho,
    }
}

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
                    Some(Layout::ZStack(children))
                }

                // Labels / Texto
                "escribir" | "etiqueta" | "label" | "text" | "Label" => {
                    if let Some(arg) = argumentos.first() {
                        match arg {
                            Expresion::Identificador { nombre: v, .. } =>
                                Some(Layout::VariableLabel { variable: v.clone() }),
                            Expresion::LiteralTexto(s) =>
                                Some(Layout::Label { texto: s.clone() }),
                            _ => Some(Layout::Label { texto: format!("{:?}", arg) }),
                        }
                    } else {
                        Some(Layout::Spacer(0.0))
                    }
                }

                "etiqueta_titulo" | "titulo" | "title" | "Title" => {
                    let texto = argumentos.first()
                        .map(|a| match a {
                            Expresion::LiteralTexto(s) => s.clone(),
                            _ => String::new(),
                        }).unwrap_or_default();
                    Some(Layout::Title(texto))
                }

                "etiqueta_dinamica" | "varlabel" | "VariableLabel" => {
                    let variable = argumentos.first()
                        .map(|a| match a {
                            Expresion::Identificador { nombre: s, .. } => s.clone(),
                            Expresion::LiteralTexto(s) => s.clone(),
                            _ => String::new(),
                        }).unwrap_or_default();
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
                    let variable = argumentos.first()
                        .map(|a| match a {
                            Expresion::LiteralTexto(s) => s.clone(),
                            Expresion::Identificador { nombre: s, .. } => s.clone(),
                            _ => String::new(),
                        }).unwrap_or_default();
                    let placeholder = argumentos.get(1)
                        .map(|a| match a {
                            Expresion::LiteralTexto(s) => s.clone(),
                            _ => String::new(),
                        }).unwrap_or_default();
                    Some(Layout::TextInput { variable, placeholder })
                }

                // Spacer
                "espacio" | "spacer" | "Spacer" => {
                    let tam = argumentos.first()
                        .and_then(|a| match a {
                            Expresion::LiteralNumero(n) => Some(*n as f64),
                            _ => None,
                        }).unwrap_or(10.0);
                    Some(Layout::Spacer(tam))
                }

                // Si es una función desconocida, mostrar como unimplemented
                _ => {
                    let args_desc: Vec<String> = argumentos.iter()
                        .map(|a| match a {
                            Expresion::LiteralTexto(s) => s.clone(),
                            Expresion::LiteralNumero(n) => n.to_string(),
                            _ => "?".to_string(),
                        }).collect();
                    Some(Layout::Unimplemented(
                        format!("{}(", nombre) + &args_desc.join(", ") + ")"
                    ))
                }
            }
        }
        Expresion::Identificador { nombre, .. } => {
            Some(Layout::VariableLabel { variable: nombre.clone() })
        }
        Expresion::LiteralTexto(s) => {
            Some(Layout::Label { texto: s.clone() })
        }
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
        if let Declaracion::Funcion {
            nombre: ref n, ..
        } = d
        {
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
    store: &VariableStore,
) -> Result<ValorGUI, String> {
    buscar_funcion(nombre, declaraciones).and_then(|func| {
        let (parametros, cuerpo) = match func {
            Declaracion::Funcion { parametros, cuerpo, .. } => (parametros, cuerpo),
            _ => return Err(format!("'{}' no es una función", nombre)),
        };
        let mut ambito = Ambito::new();

        for (i, param) in parametros.iter().enumerate() {
            if let Some(val) = args.get(i) {
                ambito.asignar(param.nombre.clone(), val.clone());
            } else if let Some(json_val) = store.get(&param.nombre) {
                ambito.asignar(param.nombre.clone(), ValorGUI::from_serde(&json_val));
            }
        }

        evaluar_bloque(cuerpo, &mut ambito, store, declaraciones)
    })
}

/// Evalúa un bloque de declaraciones
fn evaluar_bloque(
    bloque: &[Declaracion],
    ambito: &mut Ambito,
    store: &VariableStore,
    declaraciones: &[Declaracion],
) -> Result<ValorGUI, String> {
    for declaracion in bloque {
        let result = evaluar_declaracion(declaracion, ambito, store, declaraciones)?;
        if es_retorno(declaracion) {
            return Ok(result);
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
    store: &VariableStore,
    declaraciones: &[Declaracion],
) -> Result<ValorGUI, String> {
    match decl {
        Declaracion::Retornar { valor } => {
            if let Some(expr) = valor {
                evaluar_expresion(expr, ambito, store, declaraciones)
            } else {
                Ok(ValorGUI::Nulo)
            }
        }

        Declaracion::Si {
            condicion,
            bloque_verdadero,
            bloque_falso,
        } => {
            let cond_val = evaluar_expresion(condicion, ambito, store, declaraciones)?;
            if cond_val.es_verdadero() {
                evaluar_bloque(bloque_verdadero, ambito, store, declaraciones)
            } else if let Some(sino_bloque) = bloque_falso {
                evaluar_bloque(sino_bloque, ambito, store, declaraciones)
            } else {
                Ok(ValorGUI::Nulo)
            }
        }

        Declaracion::Mientras { condicion, bloque } => {
            loop {
                let cond_val = evaluar_expresion(condicion, ambito, store, declaraciones)?;
                if !cond_val.es_verdadero() {
                    break;
                }
                let result = evaluar_bloque(bloque, ambito, store, declaraciones)?;
                if !matches!(result, ValorGUI::Nulo) {
                    return Ok(result);
                }
            }
            Ok(ValorGUI::Nulo)
        }

        Declaracion::Variable { nombre, valor, .. } => {
            let val = if let Some(expr) = valor {
                evaluar_expresion(expr, ambito, store, declaraciones)?
            } else {
                ValorGUI::Nulo
            };
            ambito.asignar(nombre.clone(), val.clone());
            store.set(nombre, val.to_json_value());
            Ok(ValorGUI::Nulo)
        }

        Declaracion::Asignacion { nombre, valor, .. } => {
            let val = evaluar_expresion(valor, ambito, store, declaraciones)?;
            ambito.asignar(nombre.clone(), val.clone());
            store.set(nombre, val.to_json_value());
            Ok(ValorGUI::Nulo)
        }

        Declaracion::LlamadaFuncion { nombre, argumentos } => {
            let mut args = Vec::new();
            for arg in argumentos {
                args.push(evaluar_expresion(arg, ambito, store, declaraciones)?);
            }
            ejecutar_funcion(nombre, &args, declaraciones, store)
        }

        Declaracion::Expresion(expr) => {
            evaluar_expresion(expr, ambito, store, declaraciones)?;
            Ok(ValorGUI::Nulo)
        }

        Declaracion::Para {
            inicializacion,
            condicion,
            incremento,
            bloque,
        } => {
            if let Some(init) = inicializacion {
                evaluar_declaracion(init, ambito, store, declaraciones)?;
            }
            loop {
                if let Some(cond) = condicion {
                    let cond_val = evaluar_expresion(cond, ambito, store, declaraciones)?;
                    if !cond_val.es_verdadero() {
                        break;
                    }
                }
                let result = evaluar_bloque(bloque, ambito, store, declaraciones)?;
                if !matches!(result, ValorGUI::Nulo) {
                    return Ok(result);
                }
                if let Some(inc) = incremento {
                    evaluar_declaracion(inc, ambito, store, declaraciones)?;
                }
            }
            Ok(ValorGUI::Nulo)
        }

        Declaracion::Cuando { condicion, cuerpo, .. } => {
            let cond_val = evaluar_expresion(condicion, ambito, store, declaraciones)?;
            if cond_val.es_verdadero() {
                evaluar_bloque(cuerpo, ambito, store, declaraciones)
            } else {
                Ok(ValorGUI::Nulo)
            }
        }

        // Tipos, funciones, clases y otras declaraciones que se ignoran en runtime
        Declaracion::Funcion { .. }
        | Declaracion::Clase { .. }
        | Declaracion::Importar(_)
        | Declaracion::Enum { .. }
        | Declaracion::Rasgo { .. }
        | Declaracion::Implementacion { .. }
        | Declaracion::AccesoMiembro { .. }
        | Declaracion::AsignacionMiembro { .. }
        | Declaracion::AsignacionIndex { .. }
        | Declaracion::AsignacionMultiple { .. }
        | Declaracion::Repetir { .. } => Ok(ValorGUI::Nulo),
    }
}

fn evaluar_expresion(
    expr: &Expresion,
    ambito: &mut Ambito,
    store: &VariableStore,
    declaraciones: &[Declaracion],
) -> Result<ValorGUI, String> {
    match expr {
        Expresion::LiteralNumero(n) => Ok(ValorGUI::Entero(*n)),
        Expresion::LiteralDecimal(f) => Ok(ValorGUI::Decimal(*f)),
        Expresion::LiteralTexto(s) => Ok(ValorGUI::Texto(s.clone())),
        Expresion::LiteralBooleano(b) => Ok(ValorGUI::Booleano(*b)),
        Expresion::LiteralNulo => Ok(ValorGUI::Nulo),

        Expresion::Identificador { nombre, .. } => {
            if let Some(val) = ambito.obtener(nombre) {
                Ok(val.clone())
            } else if let Some(json_val) = store.get(nombre) {
                Ok(ValorGUI::from_serde(&json_val))
            } else {
                Err(format!("Variable '{}' no encontrada", nombre))
            }
        }

        Expresion::Binaria { izquierda, operador, derecha } => {
            let izq = evaluar_expresion(izquierda, ambito, store, declaraciones)?;
            let der = evaluar_expresion(derecha, ambito, store, declaraciones)?;
            evaluar_binaria(izq, operador, der)
        }

        Expresion::Unaria { operador, expr: inner } => {
            let val = evaluar_expresion(inner, ambito, store, declaraciones)?;
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
                args.push(evaluar_expresion(arg, ambito, store, declaraciones)?);
            }
            ejecutar_funcion(nombre, &args, declaraciones, store)
        }

        Expresion::AccesoMiembro { objeto, miembro } => {
            let obj = evaluar_expresion(objeto, ambito, store, declaraciones)?;
            let key = format!("{}_{}", obj.to_display(), miembro);
            if let Some(json_val) = store.get(&key) {
                Ok(ValorGUI::from_serde(&json_val))
            } else {
                Ok(ValorGUI::Nulo)
            }
        }

        Expresion::Grupo(inner) => {
            evaluar_expresion(inner, ambito, store, declaraciones)
        }

        Expresion::Asignacion { variable, valor } => {
            let val = evaluar_expresion(valor, ambito, store, declaraciones)?;
            ambito.asignar(variable.clone(), val.clone());
            store.set(variable, val.to_json_value());
            Ok(val)
        }

        Expresion::Arreglo(elementos) => {
            let mut values = Vec::new();
            for elem in elementos {
                values.push(evaluar_expresion(elem, ambito, store, declaraciones)?);
            }
            let json_arr: Vec<serde_json::Value> =
                values.iter().map(|v| v.to_json_value()).collect();
            let json_str = serde_json::to_string(&json_arr)
                .map_err(|e| format!("Error serializando array: {}", e))?;
            Ok(ValorGUI::Texto(json_str))
        }

        Expresion::Mapa(pares) => {
            let mut map = serde_json::Map::new();
            for (k, v) in pares {
                let key_val = evaluar_expresion(k, ambito, store, declaraciones)?;
                let val = evaluar_expresion(v, ambito, store, declaraciones)?;
                map.insert(key_val.to_display(), val.to_json_value());
            }
            let json_str = serde_json::to_string(&serde_json::Value::Object(map))
                .map_err(|e| format!("Error serializando mapa: {}", e))?;
            Ok(ValorGUI::Texto(json_str))
        }

        Expresion::Ok(inner) => {
            evaluar_expresion(inner, ambito, store, declaraciones)
        }

        Expresion::Error(inner) => {
            let val = evaluar_expresion(inner, ambito, store, declaraciones)?;
            Err(format!("Error: {}", val.to_display()))
        }

        Expresion::Algo(inner) => {
            evaluar_expresion(inner, ambito, store, declaraciones)
        }

        Expresion::Referencia { expr: inner, .. } => {
            evaluar_expresion(inner, ambito, store, declaraciones)
        }

        Expresion::Index { objeto, indice } => {
            let obj = evaluar_expresion(objeto, ambito, store, declaraciones)?;
            let idx = evaluar_expresion(indice, ambito, store, declaraciones)?;
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
                    Err(format!("Índice {} fuera de rango (len={})", idx_num, arr.len()))
                }
            } else {
                Err("No se puede indexar un valor que no es un array".to_string())
            }
        }

        // No implementados en runtime
        Expresion::Closure { .. } => Err("Closures no soportados en WASM GUI".to_string()),
        Expresion::Hilo { .. } => Err("Hilos no soportados en WASM GUI".to_string()),
        Expresion::CanalNuevo => Err("Canales no soportados en WASM GUI".to_string()),
        Expresion::Seleccionar { .. } => Err("Seleccionar no soportado en WASM GUI".to_string()),
        Expresion::Try(inner) => {
            let val = evaluar_expresion(inner, ambito, store, declaraciones)?;
            if matches!(val, ValorGUI::Nulo) {
                Err("Error propagado?".to_string())
            } else {
                Ok(val)
            }
        }
        Expresion::Resultado => Ok(ValorGUI::Nulo),
        Expresion::Anterior(inner) => evaluar_expresion(inner, ambito, store, declaraciones),

        // No implementados
        Expresion::Instanciacion { .. } => Err("Instanciación no soportada en WASM GUI".to_string()),
        Expresion::LiteralExacto(_, _) => Err("Literal exacto no soportado".to_string()),
        Expresion::AsignacionCampo { .. } => Err("Asignación de campo no soportada".to_string()),
        Expresion::ArraySet { .. } => Err("ArraySet no soportado".to_string()),
        Expresion::Coincidir { .. } => Err("Match no soportado en WASM GUI".to_string()),
    }
}

fn evaluar_binaria(
    izq: ValorGUI,
    operador: &Operador,
    der: ValorGUI,
) -> Result<ValorGUI, String> {
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
            if b == 0 { ValorGUI::Nulo } else { ValorGUI::Entero(a / b) }
        }
        (ValorGUI::Decimal(a), ValorGUI::Decimal(b)) => {
            if b == 0.0 { ValorGUI::Nulo } else { ValorGUI::Decimal(a / b) }
        }
        (ValorGUI::Entero(a), ValorGUI::Decimal(b)) => {
            if b == 0.0 { ValorGUI::Nulo } else { ValorGUI::Decimal(a as f64 / b) }
        }
        (ValorGUI::Decimal(a), ValorGUI::Entero(b)) => {
            if b == 0 { ValorGUI::Nulo } else { ValorGUI::Decimal(a / b as f64) }
        }
        _ => ValorGUI::Nulo,
    }
}

// ═══════════════════════════════════════════════════════════════════
// INICIALIZACIÓN DE ESTADO
// ═══════════════════════════════════════════════════════════════════

/// Inicializa el estado evaluando la función `main` (variables iniciales)
fn inicializar_estado(
    declaraciones: &[Declaracion],
    store: &VariableStore,
) {
    for decl in declaraciones {
        if let Declaracion::Funcion { nombre, cuerpo, .. } = decl {
            if nombre == "main" {
                let mut ambito = Ambito::new();
                for d in cuerpo {
                    if let Declaracion::Variable { nombre, valor, .. } = d {
                        let val = if let Some(expr) = valor {
                            match expr {
                                Expresion::LiteralNumero(n) => ValorGUI::Entero(*n),
                                Expresion::LiteralDecimal(f) => ValorGUI::Decimal(*f),
                                Expresion::LiteralTexto(s) => ValorGUI::Texto(s.clone()),
                                Expresion::LiteralBooleano(b) => ValorGUI::Booleano(*b),
                                Expresion::LiteralNulo => ValorGUI::Nulo,
                                _ => {
                                    match evaluar_expresion(expr, &mut ambito, store, declaraciones) {
                                        Ok(v) => v,
                                        Err(_) => ValorGUI::Nulo,
                                    }
                                }
                            }
                        } else {
                            ValorGUI::Nulo
                        };
                        ambito.asignar(nombre.clone(), val.clone());
                        store.set(nombre, val.to_json_value());
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

/// Procesa un click en coordenadas (x, y) del canvas.
/// Busca el widget en hit_areas, y si es un botón ejecuta su callback.
fn procesar_click(x: f64, y: f64) {
    APP_STATE.with(|state| {
        let state = state.borrow_mut();
        if state.declaraciones.is_empty() {
            return;
        }

        // Buscar widget clickeado (de atrás hacia adelante para Z-order)
        for area in state.hit_areas.iter().rev() {
            if x >= area.x && x <= area.x + area.ancho
                && y >= area.y && y <= area.y + area.alto
            {
                match &area.layout {
                    Layout::Button { callback, .. } if !callback.is_empty() => {
                        web_sys::console::log_1(
                            &format!("Click en botón, callback: {}", callback).into()
                        );
                        // Ejecutar la función callback
                        let args: Vec<ValorGUI> = Vec::new();
                        match ejecutar_funcion(
                            callback,
                            &args,
                            &state.declaraciones,
                            &state.store,
                        ) {
                            Ok(_) => {
                                web_sys::console::log_1(
                                    &format!("Callback '{}' ejecutado OK", callback).into()
                                );
                            }
                            Err(e) => {
                                web_sys::console::log_1(
                                    &format!("Error en callback '{}': {}", callback, e).into()
                                );
                            }
                        }
                    }
                    Layout::TextInput { variable, .. } => {
                        // Para TextInput, mostrar prompt en consola por ahora
                        web_sys::console::log_1(
                            &format!("Click en TextInput '{}'", variable).into()
                        );
                        // En una implementación futura, esto abriría un prompt
                        // o conectaría con un input HTML oculto
                    }
                    _ => {}
                }
                break;
            }
        }
    });
}

/// Configura event listeners en el canvas para clicks
fn configurar_eventos_canvas(canvas_id: &str) {
    let document = web_sys::window().unwrap().document().unwrap();
    let canvas = document.get_element_by_id(canvas_id)
        .unwrap()
        .dyn_into::<HtmlCanvasElement>()
        .unwrap();

    // Closure para click
    let closure_click = Closure::wrap(Box::new(move |event: web_sys::MouseEvent| {
        let x = event.offset_x() as f64;
        let y = event.offset_y() as f64;
        web_sys::console::log_1(&format!("Click en ({}, {})", x, y).into());
        procesar_click(x, y);
    }) as Box<dyn FnMut(_)>);

    canvas.add_event_listener_with_callback("click", closure_click.as_ref().unchecked_ref()).ok();
    closure_click.forget();
}

// ═══════════════════════════════════════════════════════════════════
// WASM EXPORTS
// ═══════════════════════════════════════════════════════════════════

/// Retorna la versión del crate
#[wasm_bindgen]
pub fn version() -> String {
    "forja-wasm-gui v0.1.0".to_string()
}

/// Renderiza un layout Forja en un canvas.
/// Compila el código, extrae el layout y lo dibuja en el canvas.
/// Retorna "ok" en éxito o un mensaje de error.
#[wasm_bindgen]
pub fn renderizar(canvas_id: &str, codigo: &str) -> String {
    // Obtener canvas y contexto
    let document = match web_sys::window() {
        Some(win) => win.document(),
        None => return "Error: no window".to_string(),
    };
    let document = match document {
        Some(d) => d,
        None => return "Error: no document".to_string(),
    };
    let canvas = match document.get_element_by_id(canvas_id) {
        Some(el) => el,
        None => return format!("Error: canvas '{}' no encontrado", canvas_id),
    };
    let canvas: HtmlCanvasElement = match canvas.dyn_into() {
        Ok(c) => c,
        Err(_) => return "Error: elemento no es un canvas".to_string(),
    };
    let ctx = match canvas.get_context("2d") {
        Ok(Some(c)) => c,
        _ => return "Error: no se pudo obtener contexto 2D".to_string(),
    };
    let ctx: CanvasRenderingContext2d = match ctx.dyn_into() {
        Ok(c) => c,
        Err(_) => return "Error: contexto no es 2D".to_string(),
    };

    let ancho = canvas.width() as f64;
    let alto = canvas.height() as f64;

    // Limpiar canvas
    ctx.clear_rect(0.0, 0.0, ancho, alto);

    // Fondo
    ctx.set_fill_style_str(COLOR_BACKGROUND);
    ctx.fill_rect(0.0, 0.0, ancho, alto);

    // Compilar código Forja
    match forja::compilar_con_ast(codigo) {
        Ok((declaraciones, _rust_code)) => {
            // Almacenar estado global
            let store = VariableStore::new();

            // Inicializar variables desde la función main
            inicializar_estado(&declaraciones, &store);

            // Extraer layout
            let layout = extraer_layout(&declaraciones);

            // Guardar estado y luego renderizar (dos pasos para evitar borrow conflicts)
            let store_clone = store.clone();
            APP_STATE.with(|state| {
                let mut s = state.borrow_mut();
                s.store = store_clone;
                s.declaraciones = declaraciones;
                s.hit_areas.clear();
                s.canvas_ancho = ancho;
                s.canvas_alto = alto;
                s.ultimo_layout = Some(layout);
            });

            // Renderizar layout (usando referencias locales)
            APP_STATE.with(|state| {
                let s = state.borrow();
                if let Some(layout_ref) = &s.ultimo_layout {
                    let mut areas_local: Vec<WidgetHitArea> = Vec::new();
                    renderizar_layout(&ctx, layout_ref, 10.0, 10.0, ancho - 20.0, &mut areas_local, &s.store);
                    // Devolver áreas para almacenar
                    drop(s);
                    let mut s = state.borrow_mut();
                    s.hit_areas = areas_local;
                }
            });

            // Configurar eventos (solo la primera vez)
            configurar_eventos_canvas(canvas_id);

            "ok".to_string()
        }
        Err(errors) => {
            let msgs: Vec<String> = errors.iter().map(|e| e.to_string()).collect();
            // Mostrar error en canvas
            ctx.set_fill_style_str(COLOR_ERROR);
            ctx.set_font("14px sans-serif");
            let mut ey = 30.0;
            for msg in &msgs {
                ctx.fill_text(msg, 10.0, ey).ok();
                ey += 20.0;
            }
            format!("Error compilando: {}", msgs.join("; "))
        }
    }
}

/// Ejecuta código Forja en modo GUI y lo renderiza.
/// Atajo para renderizar + eventos.
#[wasm_bindgen]
pub fn ejecutar_gui(canvas_id: &str, codigo: &str) -> String {
    renderizar(canvas_id, codigo)
}

/// Fuerza un re-renderizado del último layout (útil después de eventos)
#[wasm_bindgen]
pub fn rerenderizar(canvas_id: &str) -> String {
    let document = match web_sys::window() {
        Some(win) => win.document(),
        None => return "Error: no window".to_string(),
    };
    let document = match document {
        Some(d) => d,
        None => return "Error: no document".to_string(),
    };
    let canvas = match document.get_element_by_id(canvas_id) {
        Some(el) => el,
        None => return format!("Error: canvas '{}' no encontrado", canvas_id),
    };
    let canvas: HtmlCanvasElement = match canvas.dyn_into() {
        Ok(c) => c,
        Err(_) => return "Error: elemento no es un canvas".to_string(),
    };
    let ctx = match canvas.get_context("2d") {
        Ok(Some(c)) => c,
        _ => return "Error: no se pudo obtener contexto 2D".to_string(),
    };
    let ctx: CanvasRenderingContext2d = match ctx.dyn_into() {
        Ok(c) => c,
        Err(_) => return "Error: contexto no es 2D".to_string(),
    };

    let ancho = canvas.width() as f64;
    let alto = canvas.height() as f64;

    // Limpiar y re-renderizar
    ctx.clear_rect(0.0, 0.0, ancho, alto);
    ctx.set_fill_style_str(COLOR_BACKGROUND);
    ctx.fill_rect(0.0, 0.0, ancho, alto);

    APP_STATE.with(|state| {
        let s = state.borrow();
        if let Some(layout_ref) = &s.ultimo_layout {
            let mut areas_local: Vec<WidgetHitArea> = Vec::new();
            renderizar_layout(&ctx, layout_ref, 10.0, 10.0, ancho - 20.0, &mut areas_local, &s.store);
            drop(s);
            let mut s = state.borrow_mut();
            s.hit_areas = areas_local;
        }
    });

    "ok".to_string()
}
