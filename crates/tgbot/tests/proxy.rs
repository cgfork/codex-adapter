use std::{process::Output, time::Duration};

use teloxide::requests::Requester;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    process::Command,
    time::timeout,
};

const TEST_TIMEOUT: Duration = Duration::from_secs(10);
const TELEGRAM_RESPONSE: &str = r#"{"ok":true,"result":{"id":123,"is_bot":true,"first_name":"Proxy Bot","username":"proxy_bot","can_join_groups":true,"can_read_all_group_messages":false,"supports_inline_queries":false,"has_main_web_app":false}}"#;

#[tokio::test]
async fn explicit_http_proxy_routes_requests_despite_environment_and_no_proxy() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_url = format!(
        "http://proxy-user:proxy-password@{}",
        listener.local_addr().unwrap()
    );
    let proxy = async {
        let (mut stream, _) = listener.accept().await.unwrap();
        let request = read_request(&mut stream).await;
        assert!(request.starts_with("POST http://telegram.invalid/bot123:test/GetMe "));
        assert!(
            request
                .to_ascii_lowercase()
                .contains("proxy-authorization: basic ")
        );
        respond(&mut stream).await;
    };
    let request = request_in_subprocess(
        Some(&proxy_url),
        Some("http://private-user:private-password@invalid.example:invalid-port"),
        "http://telegram.invalid",
        "*",
    );
    let (_, output) = timeout(TEST_TIMEOUT, async { tokio::join!(proxy, request) })
        .await
        .expect("HTTP proxy request timed out");
    assert_success(output);
}

#[tokio::test]
async fn teloxide_proxy_fallback_routes_requests_through_http_proxy() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_url = format!("http://{}", listener.local_addr().unwrap());
    let proxy = async {
        let (mut stream, _) = listener.accept().await.unwrap();
        let request = read_request(&mut stream).await;
        assert!(request.starts_with("POST http://telegram.invalid/bot123:test/GetMe "));
        respond(&mut stream).await;
    };
    let request = request_in_subprocess(None, Some(&proxy_url), "http://telegram.invalid", "*");
    let (_, output) = timeout(TEST_TIMEOUT, async { tokio::join!(proxy, request) })
        .await
        .expect("TELOXIDE_PROXY fallback request timed out");
    assert_success(output);
}

#[tokio::test]
async fn socks5h_proxy_receives_the_remote_dns_name() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_url = format!("socks5h://{}", listener.local_addr().unwrap());
    let proxy = async {
        let (mut stream, _) = listener.accept().await.unwrap();
        socks5_greeting(&mut stream).await;
        let mut header = [0; 4];
        stream.read_exact(&mut header).await.unwrap();
        assert_eq!(header, [5, 1, 0, 3]);
        let length = stream.read_u8().await.unwrap();
        let mut host = vec![0; usize::from(length)];
        stream.read_exact(&mut host).await.unwrap();
        assert_eq!(host, b"telegram.invalid");
        assert_eq!(stream.read_u16().await.unwrap(), 43210);
        socks5_connected(&mut stream).await;
        let request = read_request(&mut stream).await;
        assert!(request.starts_with("POST /bot123:test/GetMe "));
        respond(&mut stream).await;
    };
    let request =
        request_in_subprocess(Some(&proxy_url), None, "http://telegram.invalid:43210", "*");
    let (_, output) = timeout(TEST_TIMEOUT, async { tokio::join!(proxy, request) })
        .await
        .expect("SOCKS5h proxy request timed out");
    assert_success(output);
}

#[tokio::test]
async fn socks5_proxy_routes_an_ip_destination() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_url = format!("socks5://{}", listener.local_addr().unwrap());
    let proxy = async {
        let (mut stream, _) = listener.accept().await.unwrap();
        socks5_greeting(&mut stream).await;
        let mut header = [0; 8];
        stream.read_exact(&mut header).await.unwrap();
        assert_eq!(header, [5, 1, 0, 1, 192, 0, 2, 123]);
        assert_eq!(stream.read_u16().await.unwrap(), 43210);
        socks5_connected(&mut stream).await;
        let request = read_request(&mut stream).await;
        assert!(request.starts_with("POST /bot123:test/GetMe "));
        respond(&mut stream).await;
    };
    let request = request_in_subprocess(Some(&proxy_url), None, "http://192.0.2.123:43210", "*");
    let (_, output) = timeout(TEST_TIMEOUT, async { tokio::join!(proxy, request) })
        .await
        .expect("SOCKS5 proxy request timed out");
    assert_success(output);
}

#[tokio::test]
async fn standard_system_proxy_is_used_without_telegram_proxy_configuration() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_url = format!("http://{}", listener.local_addr().unwrap());
    let proxy = async {
        let (mut stream, _) = listener.accept().await.unwrap();
        let request = read_request(&mut stream).await;
        assert!(request.starts_with("POST http://telegram.invalid/bot123:test/GetMe "));
        respond(&mut stream).await;
    };
    let request = request_in_subprocess_with_system_proxy(&proxy_url);
    let (_, output) = timeout(TEST_TIMEOUT, async { tokio::join!(proxy, request) })
        .await
        .expect("system proxy request timed out");
    assert_success(output);
}

#[tokio::test]
#[ignore = "helper invoked in a subprocess with isolated environment"]
async fn proxy_request_subprocess() {
    let proxy = std::env::var("TEST_TELEGRAM_PROXY").ok();
    let api_url = std::env::var("TEST_TELEGRAM_API_URL").unwrap();
    let bot = tgbot::create_bot("123:test", proxy.as_deref())
        .unwrap()
        .set_api_url(reqwest::Url::parse(&api_url).unwrap());
    let me = timeout(TEST_TIMEOUT, bot.get_me()).await.unwrap().unwrap();
    assert_eq!(me.user.id.0, 123);
    assert_eq!(me.user.username.as_deref(), Some("proxy_bot"));
}

fn subprocess_command(api_url: &str, no_proxy: &str) -> Command {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--exact",
            "proxy_request_subprocess",
            "--ignored",
            "--nocapture",
        ])
        .env("TEST_TELEGRAM_API_URL", api_url)
        .env_remove("TEST_TELEGRAM_PROXY")
        .env_remove("TELOXIDE_PROXY")
        .env("NO_PROXY", no_proxy)
        .env("no_proxy", no_proxy)
        .kill_on_drop(true);
    for variable in [
        "HTTP_PROXY",
        "http_proxy",
        "HTTPS_PROXY",
        "https_proxy",
        "ALL_PROXY",
        "all_proxy",
    ] {
        command.env(variable, "http://127.0.0.1:9");
    }
    command
}

async fn request_in_subprocess(
    explicit_proxy: Option<&str>,
    environment_proxy: Option<&str>,
    api_url: &str,
    no_proxy: &str,
) -> Output {
    let mut command = subprocess_command(api_url, no_proxy);
    if let Some(proxy) = explicit_proxy {
        command.env("TEST_TELEGRAM_PROXY", proxy);
    }
    if let Some(proxy) = environment_proxy {
        command.env("TELOXIDE_PROXY", proxy);
    }
    command.output().await.unwrap()
}

async fn request_in_subprocess_with_system_proxy(proxy_url: &str) -> Output {
    subprocess_command("http://telegram.invalid", "")
        .env("HTTP_PROXY", proxy_url)
        .env("http_proxy", proxy_url)
        .output()
        .await
        .unwrap()
}

fn assert_success(output: Output) {
    assert!(
        output.status.success(),
        "proxy request failed: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

async fn socks5_greeting(stream: &mut TcpStream) {
    assert_eq!(stream.read_u8().await.unwrap(), 5);
    let length = stream.read_u8().await.unwrap();
    let mut methods = vec![0; usize::from(length)];
    stream.read_exact(&mut methods).await.unwrap();
    assert!(methods.contains(&0));
    stream.write_all(&[5, 0]).await.unwrap();
}

async fn socks5_connected(stream: &mut TcpStream) {
    stream
        .write_all(&[5, 0, 0, 1, 127, 0, 0, 1, 0, 0])
        .await
        .unwrap();
}

async fn read_request(stream: &mut TcpStream) -> String {
    let mut headers = Vec::new();
    while !headers.ends_with(b"\r\n\r\n") {
        assert!(headers.len() < 8192, "request headers are too large");
        headers.push(stream.read_u8().await.unwrap());
    }
    let headers = String::from_utf8(headers).unwrap();
    let length = headers
        .lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .map(|(_, value)| value.trim().parse::<usize>().unwrap())
        .unwrap_or(0);
    assert!(length < 8192, "request body is too large");
    let mut body = vec![0; length];
    stream.read_exact(&mut body).await.unwrap();
    headers
}

async fn respond(stream: &mut TcpStream) {
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{TELEGRAM_RESPONSE}",
        TELEGRAM_RESPONSE.len(),
    );
    stream.write_all(response.as_bytes()).await.unwrap();
    stream.shutdown().await.unwrap();
}
