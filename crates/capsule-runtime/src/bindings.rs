pub mod host {
    wasmtime::component::bindgen!({
        path: "wit/host",
        world: "runtime-host",
    });
}

pub mod hook {
    wasmtime::component::bindgen!({
        path: "wit/hook",
        world: "hook",
    });
}
