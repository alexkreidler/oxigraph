#![deny(
    future_incompatible,
    nonstandard_style,
    rust_2018_idioms,
    missing_copy_implementations,
    trivial_casts,
    trivial_numeric_casts,
    unsafe_code,
    unused_qualifications
)]

use argh::FromArgs;
use async_std::future::Future;
use async_std::io::Read;
use async_std::net::{TcpListener, TcpStream};
use async_std::prelude::*;
use async_std::task::{block_on, spawn};
use http_types::{
    bail_status, headers, Body, Error, Method, Mime, Request, Response, Result, StatusCode,
};
use oxigraph::io::{DatasetFormat, GraphFormat};
use oxigraph::model::{GraphName, NamedNode, NamedOrBlankNode};
use oxigraph::sparql::algebra::GraphUpdateOperation;
use oxigraph::sparql::{Query, QueryResults, QueryResultsFormat, Update};
#[cfg(feature = "rocksdb")]
use oxigraph::RocksDbStore as Store;
#[cfg(all(feature = "sled", not(feature = "rocksdb")))]
use oxigraph::SledStore as Store;
use std::io::BufReader;
use std::str::FromStr;
use url::form_urlencoded;

const MAX_SPARQL_BODY_SIZE: u64 = 1_048_576;
const HTML_ROOT_PAGE: &str = include_str!("../templates/query.html");
const LOGO: &str = include_str!("../../logo.svg");
const SERVER: &str = concat!("Oxigraph/", env!("CARGO_PKG_VERSION"));

#[derive(FromArgs)]
/// Oxigraph SPARQL server
struct Args {
    /// specify a server socket to bind using the format $(HOST):$(PORT)
    #[argh(option, short = 'b', default = "\"localhost:7878\".to_string()")]
    bind: String,

    /// directory in which persist the data
    #[argh(option, short = 'f')]
    file: String,
}

#[async_std::main]
pub async fn main() -> Result<()> {
    let args: Args = argh::from_env();
    let store = Store::open(args.file)?;

    println!("Listening for requests at http://{}", &args.bind);
    http_server(&args.bind, move |request| {
        handle_request(request, store.clone())
    })
    .await
}

async fn handle_request(request: Request, store: Store) -> Result<Response> {
    let mut response = match (request.url().path(), request.method()) {
        ("/", Method::Get) => {
            let mut response = Response::new(StatusCode::Ok);
            response.append_header(headers::CONTENT_TYPE, "text/html");
            response.set_body(HTML_ROOT_PAGE);
            response
        }
        ("/logo.svg", Method::Get) => {
            let mut response = Response::new(StatusCode::Ok);
            response.append_header(headers::CONTENT_TYPE, "image/svg+xml");
            response.set_body(LOGO);
            response
        }
        ("/", Method::Post) => {
            if let Some(content_type) = request.content_type() {
                match if let Some(format) = GraphFormat::from_media_type(content_type.essence()) {
                    store.load_graph(
                        BufReader::new(SyncAsyncReader::from(request)),
                        format,
                        &GraphName::DefaultGraph,
                        None,
                    )
                } else if let Some(format) = DatasetFormat::from_media_type(content_type.essence())
                {
                    store.load_dataset(BufReader::new(SyncAsyncReader::from(request)), format, None)
                } else {
                    return Ok(simple_response(
                        StatusCode::UnsupportedMediaType,
                        format!("No supported content Content-Type given: {}", content_type),
                    ));
                } {
                    Ok(()) => Response::new(StatusCode::NoContent),
                    Err(error) => {
                        return Err(bad_request(error));
                    }
                }
            } else {
                simple_response(StatusCode::BadRequest, "No Content-Type given")
            }
        }
        ("/query", Method::Get) => {
            configure_and_evaluate_sparql_query(store, url_query(&request), None, request)?
        }
        ("/query", Method::Post) => {
            if let Some(content_type) = request.content_type() {
                if content_type.essence() == "application/sparql-query" {
                    let mut buffer = String::new();
                    let mut request = request;
                    request
                        .take_body()
                        .take(MAX_SPARQL_BODY_SIZE)
                        .read_to_string(&mut buffer)
                        .await?;
                    configure_and_evaluate_sparql_query(
                        store,
                        url_query(&request),
                        Some(buffer),
                        request,
                    )?
                } else if content_type.essence() == "application/x-www-form-urlencoded" {
                    let mut buffer = Vec::new();
                    let mut request = request;
                    request
                        .take_body()
                        .take(MAX_SPARQL_BODY_SIZE)
                        .read_to_end(&mut buffer)
                        .await?;
                    configure_and_evaluate_sparql_query(store, buffer, None, request)?
                } else {
                    simple_response(
                        StatusCode::UnsupportedMediaType,
                        format!("Not supported Content-Type given: {}", content_type),
                    )
                }
            } else {
                simple_response(StatusCode::BadRequest, "No Content-Type given")
            }
        }
        ("/update", Method::Post) => {
            if let Some(content_type) = request.content_type() {
                if content_type.essence() == "application/sparql-update" {
                    let mut buffer = String::new();
                    let mut request = request;
                    request
                        .take_body()
                        .take(MAX_SPARQL_BODY_SIZE)
                        .read_to_string(&mut buffer)
                        .await?;
                    configure_and_evaluate_sparql_update(
                        store,
                        url_query(&request),
                        Some(buffer),
                        request,
                    )?
                } else if content_type.essence() == "application/x-www-form-urlencoded" {
                    let mut buffer = Vec::new();
                    let mut request = request;
                    request
                        .take_body()
                        .take(MAX_SPARQL_BODY_SIZE)
                        .read_to_end(&mut buffer)
                        .await?;
                    configure_and_evaluate_sparql_update(store, buffer, None, request)?
                } else {
                    simple_response(
                        StatusCode::UnsupportedMediaType,
                        format!("Not supported Content-Type given: {}", content_type),
                    )
                }
            } else {
                simple_response(StatusCode::BadRequest, "No Content-Type given")
            }
        }
        _ => Response::new(StatusCode::NotFound),
    };
    response.append_header(headers::SERVER, SERVER);
    Ok(response)
}

fn simple_response(status: StatusCode, body: impl Into<Body>) -> Response {
    let mut response = Response::new(status);
    response.set_body(body);
    response
}

fn base_url(request: &Request) -> &str {
    let url = request.url().as_str();
    url.split('?').next().unwrap_or(url)
}

fn url_query(request: &Request) -> Vec<u8> {
    request.url().query().unwrap_or("").as_bytes().to_vec()
}

fn configure_and_evaluate_sparql_query(
    store: Store,
    encoded: Vec<u8>,
    mut query: Option<String>,
    request: Request,
) -> Result<Response> {
    let mut default_graph_uris = Vec::new();
    let mut named_graph_uris = Vec::new();
    for (k, v) in form_urlencoded::parse(&encoded) {
        match k.as_ref() {
            "query" => {
                if query.is_some() {
                    bail_status!(400, "Multiple query parameters provided")
                }
                query = Some(v.into_owned())
            }
            "default-graph-uri" => default_graph_uris.push(v.into_owned()),
            "named-graph-uri" => named_graph_uris.push(v.into_owned()),
            _ => {
                return Ok(simple_response(
                    StatusCode::BadRequest,
                    format!("Unexpected parameter: {}", k),
                ))
            }
        }
    }
    if let Some(query) = query {
        evaluate_sparql_query(store, query, default_graph_uris, named_graph_uris, request)
    } else {
        Ok(simple_response(
            StatusCode::BadRequest,
            "You should set the 'query' parameter",
        ))
    }
}

fn evaluate_sparql_query(
    store: Store,
    query: String,
    default_graph_uris: Vec<String>,
    named_graph_uris: Vec<String>,
    request: Request,
) -> Result<Response> {
    let mut query = Query::parse(&query, Some(base_url(&request))).map_err(bad_request)?;
    let default_graph_uris = default_graph_uris
        .into_iter()
        .map(|e| Ok(NamedNode::new(e)?.into()))
        .collect::<Result<Vec<GraphName>>>()
        .map_err(bad_request)?;
    let named_graph_uris = named_graph_uris
        .into_iter()
        .map(|e| Ok(NamedNode::new(e)?.into()))
        .collect::<Result<Vec<NamedOrBlankNode>>>()
        .map_err(bad_request)?;

    if !default_graph_uris.is_empty() || !named_graph_uris.is_empty() {
        query.dataset_mut().set_default_graph(default_graph_uris);
        query
            .dataset_mut()
            .set_available_named_graphs(named_graph_uris);
    }

    let results = store.query(query)?;
    //TODO: stream
    if let QueryResults::Graph(_) = results {
        let format = content_negotiation(
            request,
            &[
                GraphFormat::NTriples.media_type(),
                GraphFormat::Turtle.media_type(),
                GraphFormat::RdfXml.media_type(),
            ],
            GraphFormat::from_media_type,
        )?;
        let mut body = Vec::default();
        results.write_graph(&mut body, format)?;
        let mut response = Response::from(body);
        response.insert_header(headers::CONTENT_TYPE, format.media_type());
        Ok(response)
    } else {
        let format = content_negotiation(
            request,
            &[
                QueryResultsFormat::Xml.media_type(),
                QueryResultsFormat::Json.media_type(),
                QueryResultsFormat::Csv.media_type(),
                QueryResultsFormat::Tsv.media_type(),
            ],
            QueryResultsFormat::from_media_type,
        )?;
        let mut body = Vec::default();
        results.write(&mut body, format)?;
        let mut response = Response::from(body);
        response.insert_header(headers::CONTENT_TYPE, format.media_type());
        Ok(response)
    }
}

fn configure_and_evaluate_sparql_update(
    store: Store,
    encoded: Vec<u8>,
    mut update: Option<String>,
    request: Request,
) -> Result<Response> {
    let mut default_graph_uris = Vec::new();
    let mut named_graph_uris = Vec::new();
    for (k, v) in form_urlencoded::parse(&encoded) {
        match k.as_ref() {
            "update" => {
                if update.is_some() {
                    bail_status!(400, "Multiple update parameters provided")
                }
                update = Some(v.into_owned())
            }
            "using-graph-uri" => default_graph_uris.push(v.into_owned()),
            "using-named-graph-uri" => named_graph_uris.push(v.into_owned()),
            _ => {
                return Ok(simple_response(
                    StatusCode::BadRequest,
                    format!("Unexpected parameter: {}", k),
                ))
            }
        }
    }
    if let Some(update) = update {
        evaluate_sparql_update(store, update, default_graph_uris, named_graph_uris, request)
    } else {
        Ok(simple_response(
            StatusCode::BadRequest,
            "You should set the 'update' parameter",
        ))
    }
}

fn evaluate_sparql_update(
    store: Store,
    update: String,
    default_graph_uris: Vec<String>,
    named_graph_uris: Vec<String>,
    request: Request,
) -> Result<Response> {
    let mut update = Update::parse(&update, Some(base_url(&request))).map_err(|e| {
        let mut e = Error::from(e);
        e.set_status(StatusCode::BadRequest);
        e
    })?;
    let default_graph_uris = default_graph_uris
        .into_iter()
        .map(|e| Ok(NamedNode::new(e)?.into()))
        .collect::<Result<Vec<GraphName>>>()
        .map_err(bad_request)?;
    let named_graph_uris = named_graph_uris
        .into_iter()
        .map(|e| Ok(NamedNode::new(e)?.into()))
        .collect::<Result<Vec<NamedOrBlankNode>>>()
        .map_err(bad_request)?;
    if !default_graph_uris.is_empty() || !named_graph_uris.is_empty() {
        for operation in &mut update.operations {
            if let GraphUpdateOperation::DeleteInsert { using, .. } = operation {
                if !using.is_default_dataset() {
                    let result = Ok(simple_response(
                        StatusCode::BadRequest,
                        "using-graph-uri and using-named-graph-uri must not be used with a SPARQL UPDATE containing USING",
                    ));
                    return result;
                }
                using.set_default_graph(default_graph_uris.clone());
                using.set_available_named_graphs(named_graph_uris.clone());
            }
        }
    }
    store.update(update)?;
    Ok(Response::new(StatusCode::NoContent))
}

async fn http_server<
    F: Clone + Send + Sync + 'static + Fn(Request) -> Fut,
    Fut: Send + Future<Output = Result<Response>>,
>(
    host: &str,
    handle: F,
) -> Result<()> {
    async fn accept<F: Fn(Request) -> Fut, Fut: Future<Output = Result<Response>>>(
        stream: TcpStream,
        handle: F,
    ) -> Result<()> {
        async_h1::accept(stream, |request| async {
            Ok(match handle(request).await {
                Ok(result) => result,
                Err(error) => simple_response(error.status(), error.to_string()),
            })
        })
        .await
    }

    let listener = TcpListener::bind(host).await?;
    let mut incoming = listener.incoming();
    while let Some(stream) = incoming.next().await {
        let stream = stream?;
        let handle = handle.clone();
        spawn(async {
            if let Err(err) = accept(stream, handle).await {
                eprintln!("{}", err);
            };
        });
    }
    Ok(())
}

fn content_negotiation<F>(
    request: Request,
    supported: &[&str],
    parse: impl Fn(&str) -> Option<F>,
) -> Result<F> {
    let header = request
        .header(headers::ACCEPT)
        .map(|h| h.last().as_str().trim())
        .unwrap_or("");
    let supported: Vec<Mime> = supported
        .iter()
        .map(|h| Mime::from_str(h).unwrap())
        .collect();

    let mut result = supported.first().unwrap();
    let mut result_score = 0f32;

    if !header.is_empty() {
        for possible in header.split(',') {
            let possible = Mime::from_str(possible.trim())?;
            let score = if let Some(q) = possible.param("q") {
                f32::from_str(&q.to_string())?
            } else {
                1.
            };
            if score <= result_score {
                continue;
            }
            for candidate in &supported {
                if (possible.basetype() == candidate.basetype() || possible.basetype() == "*")
                    && (possible.subtype() == candidate.subtype() || possible.subtype() == "*")
                {
                    result = candidate;
                    result_score = score;
                    break;
                }
            }
        }
    }

    parse(result.essence())
        .ok_or_else(|| Error::from_str(StatusCode::InternalServerError, "Unknown mime type"))
}

fn bad_request(e: impl Into<Error>) -> Error {
    let mut e = e.into();
    e.set_status(StatusCode::BadRequest);
    e
}

struct SyncAsyncReader<R: Unpin> {
    inner: R,
}

impl<R: Unpin> From<R> for SyncAsyncReader<R> {
    fn from(inner: R) -> Self {
        Self { inner }
    }
}

impl<R: Read + Unpin> std::io::Read for SyncAsyncReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        block_on(self.inner.read(buf))
    }

    //TODO: implement other methods
}

#[cfg(test)]
mod tests {
    use super::Store;
    use crate::handle_request;
    use async_std::task::block_on;
    use http_types::{Method, Request, StatusCode, Url};
    use std::collections::hash_map::DefaultHasher;
    use std::env::temp_dir;
    use std::fs::remove_dir_all;
    use std::hash::{Hash, Hasher};

    #[test]
    fn get_ui() {
        exec(
            Request::new(Method::Get, Url::parse("http://localhost/").unwrap()),
            StatusCode::Ok,
        )
    }

    #[test]
    fn post_file() {
        let mut request = Request::new(Method::Post, Url::parse("http://localhost/").unwrap());
        request.insert_header("Content-Type", "text/turtle");
        request.set_body("<http://example.com> <http://example.com> <http://example.com> .");
        exec(request, StatusCode::NoContent)
    }

    #[test]
    fn post_wrong_file() {
        let mut request = Request::new(Method::Post, Url::parse("http://localhost/").unwrap());
        request.insert_header("Content-Type", "text/turtle");
        request.set_body("<http://example.com>");
        exec(request, StatusCode::BadRequest)
    }

    #[test]
    fn post_unsupported_file() {
        let mut request = Request::new(Method::Post, Url::parse("http://localhost/").unwrap());
        request.insert_header("Content-Type", "text/foo");
        exec(request, StatusCode::UnsupportedMediaType)
    }

    #[test]
    fn get_query() {
        exec(
            Request::new(
                Method::Get,
                Url::parse(
                    "http://localhost/query?query=SELECT%20*%20WHERE%20{%20?s%20?p%20?o%20}",
                )
                .unwrap(),
            ),
            StatusCode::Ok,
        );
    }

    #[test]
    fn get_bad_query() {
        exec(
            Request::new(
                Method::Get,
                Url::parse("http://localhost/query?query=SELECT").unwrap(),
            ),
            StatusCode::BadRequest,
        );
    }

    #[test]
    fn get_without_query() {
        exec(
            Request::new(Method::Get, Url::parse("http://localhost/query").unwrap()),
            StatusCode::BadRequest,
        );
    }

    #[test]
    fn post_query() {
        let mut request = Request::new(Method::Post, Url::parse("http://localhost/query").unwrap());
        request.insert_header("Content-Type", "application/sparql-query");
        request.set_body("SELECT * WHERE { ?s ?p ?o }");
        exec(request, StatusCode::Ok)
    }

    #[test]
    fn post_bad_query() {
        let mut request = Request::new(Method::Post, Url::parse("http://localhost/query").unwrap());
        request.insert_header("Content-Type", "application/sparql-query");
        request.set_body("SELECT");
        exec(request, StatusCode::BadRequest)
    }

    #[test]
    fn post_unknown_query() {
        let mut request = Request::new(Method::Post, Url::parse("http://localhost/query").unwrap());
        request.insert_header("Content-Type", "application/sparql-todo");
        request.set_body("SELECT");
        exec(request, StatusCode::UnsupportedMediaType)
    }

    #[test]
    fn post_federated_query() {
        let mut request = Request::new(Method::Post, Url::parse("http://localhost/query").unwrap());
        request.insert_header("Content-Type", "application/sparql-query");
        request.set_body("SELECT * WHERE { SERVICE <https://query.wikidata.org/sparql> { <https://en.wikipedia.org/wiki/Paris> ?p ?o } }");
        exec(request, StatusCode::Ok)
    }

    #[test]
    fn post_update() {
        let mut request =
            Request::new(Method::Post, Url::parse("http://localhost/update").unwrap());
        request.insert_header("Content-Type", "application/sparql-update");
        request.set_body(
            "INSERT DATA { <http://example.com> <http://example.com> <http://example.com> }",
        );
        exec(request, StatusCode::NoContent)
    }

    #[test]
    fn post_bad_update() {
        let mut request =
            Request::new(Method::Post, Url::parse("http://localhost/update").unwrap());
        request.insert_header("Content-Type", "application/sparql-update");
        request.set_body("INSERT");
        exec(request, StatusCode::BadRequest)
    }

    fn exec(request: Request, expected_status: StatusCode) {
        let mut path = temp_dir();
        path.push("temp-oxigraph-server-test");
        let mut s = DefaultHasher::new();
        format!("{:?}", request).hash(&mut s);
        path.push(&s.finish().to_string());

        let store = Store::open(&path).unwrap();
        let (code, message) = match block_on(handle_request(request, store)) {
            Ok(r) => (r.status(), "".to_string()),
            Err(e) => (e.status(), e.to_string()),
        };
        assert_eq!(code, expected_status, "Error message: {}", message);
        remove_dir_all(&path).unwrap()
    }
}
