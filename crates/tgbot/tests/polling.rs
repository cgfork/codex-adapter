use std::time::Duration;

use codebridge_core::{Bot as _, IncomingMessage};
use teloxide::{
    Bot,
    types::{ChatId, UserId},
};
use tgbot::TelegramBot;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::mpsc,
    time::timeout,
};

async fn read_request(stream: &mut tokio::net::TcpStream) -> (String, String) {
    let mut bytes = Vec::new();
    while !bytes.ends_with(b"\r\n\r\n") {
        let mut byte = [0];
        stream.read_exact(&mut byte).await.unwrap();
        bytes.push(byte[0]);
    }
    let headers = String::from_utf8(bytes).unwrap();
    let length = headers
        .lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .map(|(_, length)| length.trim().parse::<usize>().unwrap())
        .unwrap_or(0);
    let mut body = vec![0; length];
    stream.read_exact(&mut body).await.unwrap();
    (
        headers.lines().next().unwrap().to_owned(),
        String::from_utf8(body).unwrap(),
    )
}

async fn reply(stream: &mut tokio::net::TcpStream, body: &str) {
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await.unwrap();
}

#[tokio::test]
async fn core_polling_normalizes_updates_and_stops_when_dropped() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (requests, mut observed) = mpsc::unbounded_channel();
    let server = tokio::spawn(async move {
        for index in 0..5 {
            let (mut stream, _) = listener.accept().await.unwrap();
            let (path, body) = read_request(&mut stream).await;
            requests.send((path, body)).unwrap();
            let result = match index {
                0 => {
                    r#"{"ok":true,"result":{"id":123,"is_bot":true,"first_name":"Bridge","username":"codebridge_bot","can_join_groups":true,"can_read_all_group_messages":false,"supports_inline_queries":false,"has_main_web_app":false}}"#
                }
                1 => r#"{"ok":true,"result":true}"#,
                2 => {
                    r#"{"ok":true,"result":{"url":"","has_custom_certificate":false,"pending_update_count":0}}"#
                }
                3 => {
                    r#"{"ok":true,"result":[{"update_id":42,"message":{"message_id":7,"date":1700000000,"chat":{"id":123,"type":"private","first_name":"Alice"},"from":{"id":123,"is_bot":false,"first_name":"Alice"},"text":"/init"}}]}"#
                }
                _ => return,
            };
            reply(&mut stream, result).await;
        }
    });
    let bot = Bot::new("123:test")
        .set_api_url(reqwest::Url::parse(&format!("http://{address}")).unwrap());
    let adapter = TelegramBot::new(bot);
    let (events, mut receiver) = mpsc::channel(2);
    let task = tokio::spawn(async move { adapter.run(events).await });

    let maybe_event = timeout(Duration::from_secs(3), receiver.recv())
        .await
        .unwrap();
    if maybe_event.is_none() {
        panic!("poller ended: {:?}", task.await);
    }
    let event = maybe_event.unwrap();
    assert!(matches!(
        event,
        IncomingMessage::Message {
            chat_id: ChatId(123),
            sender: Some(UserId(123)),
            command: Some(codebridge_core::Command::Init),
            ..
        }
    ));
    let mut calls = Vec::new();
    for _ in 0..5 {
        calls.push(
            timeout(Duration::from_secs(3), observed.recv())
                .await
                .unwrap()
                .unwrap(),
        );
    }
    assert!(calls[0].0.contains("GetMe"));
    assert!(calls[1].0.contains("SetMyCommands"));
    assert!(calls[2].0.contains("GetWebhookInfo"));
    assert!(calls[3].0.contains("GetUpdates"));
    assert!(calls[3].1.contains("callback_query"));
    assert!(calls[3].1.contains("message"));
    assert!(calls[4].1.contains("\"offset\":43"));

    task.abort();
    task.await.unwrap_err();
    server.await.unwrap();
}
