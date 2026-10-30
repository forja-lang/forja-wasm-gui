# forja-wasm-gui

Runtime GUI de **Forja (fa)** compilado a **WebAssembly**: renderiza interfaces
Forja en un `<canvas>` HTML5 usando la API Canvas 2D.

[![CI](https://github.com/forja-lang/forja-wasm-gui/actions/workflows/ci.yml/badge.svg)](https://github.com/forja-lang/forja-wasm-gui/actions/workflows/ci.yml)

## Qué hace

- Compila código Forja a AST con `forja::transpilar_a_rust_con_ast()`.
- Extrae el layout declarativo de la función `main` y lo dibuja en Canvas 2D.
- Mantiene un `VariableStore` para las variables de la interfaz.
- Hace hit-testing de clicks sobre los widgets y ejecuta callbacks Forja con un
  evaluador tree-walking simplificado.
- **No** depende de `winit`, `xilem`, `masonry` ni `vello`: es una alternativa
  liviana para la web/playground.

## API WASM

| Función | Descripción |
|---|---|
| `version()` | Versión del runtime. |
| `renderizar(canvas_id, codigo)` | Compila y dibuja el layout en el canvas. Devuelve `"ok"` o un error. |
| `ejecutar_gui(canvas_id, codigo)` | Alias de `renderizar` orientado a ejecución. |
| `rerenderizar(canvas_id)` | Vuelve a dibujar el último layout (tras un evento). |

## Uso desde JavaScript

```html
<canvas id="app" width="800" height="600"></canvas>
<script type="module">
  import init, { renderizar } from "./pkg/forja_wasm_gui.js";

  await init();
  const resultado = renderizar("app", `
    funcion main() {
        columna_centrada(
            titular_grande("Hola desde Forja"),
            boton_relleno("Aceptar", &al_aceptar)
        )
    }
  `);
  console.log(resultado); // "ok"
</script>
```

## Compilar

Requiere `wasm-pack` y el target `wasm32-unknown-unknown`.

```bash
# Desde la raíz del repo forja
wasm-pack build crates/forja-wasm-gui \
  --target web \
  --out-dir ../../wasm-output-gui \
  --out-name forja_wasm_gui \
  --no-opt

# Opcional: optimizar el .wasm
wasm-opt --enable-bulk-memory --enable-nontrapping-float-to-int \
  --enable-sign-ext -O3 \
  wasm-output-gui/forja_wasm_gui_bg.wasm \
  -o wasm-output-gui/forja_wasm_gui_bg.wasm
```

El directorio `pkg/` contiene un build previo listo para consumir.

## Tests y verificación

El crate no tiene tests unitarios (depende de APIs del navegador). El CI verifica
que compile contra el target real:

```bash
cargo check -p forja-wasm-gui --target wasm32-unknown-unknown --locked
```

El workflow del repo clona el workspace Forja, superpone este crate y ejecuta
`fmt`, `clippy` y el `check` para `wasm32-unknown-unknown`.

## Estado

Versión `0.9.5`; el consumo es por ruta desde el
workspace [forja-lang/forja](https://github.com/forja-lang/forja).

## Licencia

GPL-3.0-or-later. Ver `LICENSE.md` en [forja-lang/forja](https://github.com/forja-lang/forja).
