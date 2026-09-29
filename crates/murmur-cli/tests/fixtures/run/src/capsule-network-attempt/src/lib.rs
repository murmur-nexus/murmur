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

    // Bounds an admitted request's attempt on the network: the host is never meant to answer.
    let options = wasi::http::types::RequestOptions::new();
    options.set_connect_timeout(Some(100_000_000)).unwrap();
    options.set_first_byte_timeout(Some(100_000_000)).unwrap();

    // The runtime refuses a request through its response, before any connection exists. Any other
    // outcome means the request was let through, whatever the network then made of it.
    let response = wasi::http::outgoing_handler::handle(request, Some(options))?;
    response.subscribe().block();
    match response.get() {
        Some(Ok(Err(wasi::http::types::ErrorCode::HttpRequestDenied))) => {
            Err(wasi::http::types::ErrorCode::HttpRequestDenied)
        }
        _ => Ok(()),
    }
}

export!(Capsule);
