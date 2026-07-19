wit_bindgen::generate!({
    path: "../../../../../../capsule-runtime/wit/guest",
    world: "capsule",
    generate_all,
});

struct Capsule;

impl exports::murmur::capsule::run::Guest for Capsule {
    fn run() {
        let result = match make_request() {
            Ok(()) => "allowed".to_string(),
            Err(wasi::http::types::ErrorCode::HttpRequestDenied) => "denied".to_string(),
            Err(err) => format!("error:{err:?}"),
        };

        std::fs::create_dir_all("./out").expect("create output directory");
        std::fs::write("./out/result.txt", result).expect("write result file");
    }
}

fn make_request() -> Result<(), wasi::http::types::ErrorCode> {
    let headers = wasi::http::types::Fields::new();
    let request = wasi::http::types::OutgoingRequest::new(headers);

    request.set_scheme(Some(&wasi::http::types::Scheme::Https)).unwrap();
    request.set_authority(Some("blocked.example.com")).unwrap();
    request.set_path_with_query(Some("/")).unwrap();

    let _future = wasi::http::outgoing_handler::handle(request, None)?;
    Ok(())
}

export!(Capsule);
