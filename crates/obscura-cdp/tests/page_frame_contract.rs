use obscura_cdp::dispatch::{dispatch, CdpContext};
use obscura_cdp::types::CdpRequest;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

async fn serve() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut buffer = [0u8; 2048];
                let read = socket.read(&mut buffer).await.unwrap_or(0);
                let request = String::from_utf8_lossy(&buffer[..read]);
                let body = if request.starts_with("GET /child.html ") {
                    "<html><body>child</body></html>"
                } else {
                    r##"<!doctype html><html><body>
                        <a id="route" href="#next">next</a>
                        <iframe src="/child.html"></iframe>
                        <script>
                            route.addEventListener('click', event => {
                                event.preventDefault();
                                history.pushState({}, '', '/next');
                            });
                        </script>
                    </body></html>"##
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
            });
        }
    });
    format!("http://{address}/")
}

async fn cdp(
    ctx: &mut CdpContext,
    id: u64,
    method: &str,
    params: Value,
    session_id: Option<&str>,
) -> Value {
    let response = dispatch(
        &CdpRequest {
            id,
            method: method.to_string(),
            params,
            session_id: session_id.map(str::to_string),
        },
        ctx,
    )
    .await;
    assert!(
        response.error.is_none(),
        "CDP {method} failed: {:?}",
        response.error
    );
    response.result.unwrap_or_else(|| json!({}))
}

fn assert_frame_contract(frame: &Value) {
    for field in [
        "id",
        "loaderId",
        "url",
        "domainAndRegistry",
        "securityOrigin",
        "mimeType",
        "secureContextType",
        "crossOriginIsolatedContextType",
    ] {
        assert!(
            frame[field].is_string(),
            "{field} is missing or not a string: {frame}"
        );
    }
    assert!(
        matches!(
            frame["secureContextType"].as_str(),
            Some("Secure" | "SecureLocalhost" | "InsecureScheme" | "InsecureAncestor")
        ),
        "invalid secureContextType: {frame}"
    );
    assert!(
        matches!(
            frame["crossOriginIsolatedContextType"].as_str(),
            Some("Isolated" | "NotIsolated" | "NotIsolatedFeatureDisabled")
        ),
        "invalid crossOriginIsolatedContextType: {frame}"
    );
    assert!(
        frame["gatedAPIFeatures"].is_array(),
        "gatedAPIFeatures is missing: {frame}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn every_page_frame_path_uses_the_current_cdp_contract() {
    std::env::set_var("OBSCURA_ALLOW_PRIVATE_NETWORK", "1");
    let mut ctx = CdpContext::new();

    let created = cdp(
        &mut ctx,
        1,
        "Target.createTarget",
        json!({"url": "about:blank"}),
        None,
    )
    .await;
    let page_id = created["targetId"].as_str().unwrap().to_string();
    let attached = cdp(
        &mut ctx,
        2,
        "Target.attachToTarget",
        json!({"targetId": page_id, "flatten": true}),
        None,
    )
    .await;
    let session_id = attached["sessionId"].as_str().unwrap().to_string();

    let initial_tree = cdp(
        &mut ctx,
        3,
        "Page.getFrameTree",
        json!({}),
        Some(&session_id),
    )
    .await;
    let initial_frame = &initial_tree["frameTree"]["frame"];
    assert_frame_contract(initial_frame);
    assert_eq!(initial_frame["loaderId"], format!("loader-blank-{page_id}"));
    assert_eq!(initial_frame["secureContextType"], "Secure");

    let navigation_event_start = ctx.pending_events.len();
    let navigated = cdp(
        &mut ctx,
        4,
        "Page.navigate",
        json!({"url": serve().await, "waitUntil": "load"}),
        Some(&session_id),
    )
    .await;
    let loader_id = navigated["loaderId"].as_str().unwrap();
    let navigation_frame = &ctx.pending_events[navigation_event_start..]
        .iter()
        .find(|event| {
            event.method == "Page.frameNavigated" && event.params["frame"]["id"] == page_id
        })
        .expect("main-frame navigation event was not emitted")
        .params["frame"];
    assert_frame_contract(navigation_frame);
    assert_eq!(navigation_frame["loaderId"], loader_id);
    assert_eq!(navigation_frame["secureContextType"], "SecureLocalhost");

    cdp(
        &mut ctx,
        5,
        "Runtime.evaluate",
        json!({"expression": "document.title", "returnByValue": true}),
        Some(&session_id),
    )
    .await;
    let loaded_tree = cdp(
        &mut ctx,
        6,
        "Page.getFrameTree",
        json!({}),
        Some(&session_id),
    )
    .await;
    let root = &loaded_tree["frameTree"];
    assert_frame_contract(&root["frame"]);
    assert_eq!(root["frame"]["loaderId"], loader_id);
    let child = &root["childFrames"][0]["frame"];
    assert_frame_contract(child);
    assert_eq!(child["parentId"], root["frame"]["id"]);
    let child_event_frame = &ctx
        .pending_events
        .iter()
        .find(|event| {
            event.method == "Page.frameNavigated" && event.params["frame"]["id"] == child["id"]
        })
        .expect("child-frame navigation event was not emitted")
        .params["frame"];
    assert_frame_contract(child_event_frame);

    cdp(
        &mut ctx,
        7,
        "Runtime.evaluate",
        json!({
            "expression": "document.elementFromPoint = () => document.getElementById('route')"
        }),
        Some(&session_id),
    )
    .await;
    cdp(
        &mut ctx,
        8,
        "Input.dispatchMouseEvent",
        json!({"type": "mousePressed", "x": 0, "y": 0, "button": "left"}),
        Some(&session_id),
    )
    .await;
    let route_event_start = ctx.pending_events.len();
    cdp(
        &mut ctx,
        9,
        "Input.dispatchMouseEvent",
        json!({"type": "mouseReleased", "x": 0, "y": 0, "button": "left"}),
        Some(&session_id),
    )
    .await;
    let route_event = &ctx.pending_events[route_event_start..]
        .iter()
        .find(|event| event.method == "Page.navigatedWithinDocument"
            && event.params["frameId"] == page_id)
        .expect("same-document navigation event was not emitted")
        .params;
    assert!(route_event["url"].as_str().unwrap().ends_with("/next"));
    assert!(!ctx.pending_events[route_event_start..].iter().any(|event|
        event.method == "Page.frameNavigated" || event.method == "Runtime.executionContextsCleared"));
    let route_tree = cdp(&mut ctx, 10, "Page.getFrameTree", json!({}), Some(&session_id)).await;
    let route_frame = &route_tree["frameTree"]["frame"];
    assert_frame_contract(route_frame);
    assert_eq!(route_frame["loaderId"], loader_id);
    assert!(route_frame["url"].as_str().unwrap().ends_with("/next"));
}

#[tokio::test(flavor = "current_thread")]
async fn evaluated_history_navigation_preserves_the_execution_context() {
    std::env::set_var("OBSCURA_ALLOW_PRIVATE_NETWORK", "1");
    let url = serve().await;
    let mut ctx = CdpContext::new();
    let page_id = ctx.create_page();
    let session = "history-context";
    ctx.sessions.insert(session.into(), page_id.clone());
    cdp(&mut ctx, 1, "Page.navigate", json!({"url":url}), Some(session)).await;
    cdp(&mut ctx, 2, "Runtime.enable", json!({}), Some(session)).await;
    let context_id = ctx.pending_events.iter().rev()
        .find(|e| e.method == "Runtime.executionContextCreated"
            && e.params["context"]["auxData"]["frameId"] == page_id
            && e.params["context"]["auxData"]["isDefault"] == true).unwrap()
        .params["context"]["id"].clone();
    ctx.pending_events.clear();
    cdp(&mut ctx, 3, "Runtime.evaluate", json!({
        "expression":"globalThis.retainedValue=17;history.pushState({},'', '/history')",
        "contextId":context_id}), Some(session)).await;
    assert!(ctx.pending_events.iter().any(|e| e.method == "Page.navigatedWithinDocument"));
    assert!(!ctx.pending_events.iter().any(|e|
        e.method == "Runtime.executionContextsCleared" || e.method == "Page.frameNavigated"));
    let result = cdp(&mut ctx, 4, "Runtime.evaluate", json!({
        "expression":"globalThis.retainedValue", "contextId":context_id,
        "returnByValue":true}), Some(session)).await;
    assert_eq!(result["result"]["value"], 17);
}

#[tokio::test(flavor = "current_thread")]
async fn clicked_data_url_emits_a_new_document_lifecycle() {
    let mut ctx = CdpContext::new();
    let page_id = ctx.create_page();
    let session = "data-navigation";
    ctx.sessions.insert(session.into(), page_id);
    cdp(&mut ctx, 1, "Page.navigate", json!({"url":"data:text/html,<body><button id=b>Go</button></body>"}), Some(session)).await;
    cdp(&mut ctx, 2, "Runtime.enable", json!({}), Some(session)).await;
    cdp(&mut ctx, 3, "Runtime.evaluate", json!({"expression":
        "document.elementFromPoint=()=>document.getElementById('b');document.getElementById('b').onclick=()=>location.href='data:text/html,<title>Destination</title><h1>Done</h1>'"}), Some(session)).await;
    ctx.pending_events.clear();
    for (id, kind) in [(4,"mousePressed"),(5,"mouseReleased")] {
        cdp(&mut ctx, id, "Input.dispatchMouseEvent", json!({"type":kind,"x":0,"y":0,"button":"left","clickCount":1}), Some(session)).await;
    }
    assert!(ctx.pending_events.iter().any(|e| e.method == "Page.frameNavigated"));
    assert!(ctx.pending_events.iter().any(|e| e.method == "Page.lifecycleEvent" && e.params["name"] == "load"));
    assert!(ctx.pending_events.iter().any(|e| e.method == "Runtime.executionContextsCleared"));
    assert!(!ctx.pending_events.iter().any(|e| e.method == "Page.navigatedWithinDocument"));
    let result = cdp(&mut ctx, 6, "Runtime.evaluate", json!({"expression":"document.title", "returnByValue":true}), Some(session)).await;
    assert_eq!(result["result"]["value"], "Destination");
}
