pub mod host {
    wasmtime::component::bindgen!({
        path: "wit/host",
        world: "runtime-host",
    });
}

/// The host side of the `tool` world's `murmur:process/process` import, served to every WASM
/// tool's store by `invoke_tool_component` through [`crate::session_process`].
pub mod tool_host {
    wasmtime::component::bindgen!({
        path: "wit/host",
        world: "tool-host",
        imports: { default: async },
        with: {
            "murmur:process/process.connection": crate::session_process::Connection,
        },
    });
}

pub mod hook {
    wasmtime::component::bindgen!({
        path: "wit/hook",
        world: "hook",
    });
}

pub mod process_driver {
    wasmtime::component::bindgen!({
        path: "wit/process-driver",
        world: "process-driver",
        exports: { default: async },
    });
}
