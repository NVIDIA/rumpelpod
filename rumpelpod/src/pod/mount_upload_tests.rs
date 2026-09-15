// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::io::{Read, Write};
use std::time::Duration;

use crate::pod::PodClient;
use axum::body::Body;
use axum::routing::post;
use axum::Router;
use flate2::read::GzDecoder;
use futures_util::StreamExt;
use rand::RngExt;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::time::timeout;
use tokio_util::task::AbortOnDropHandle;

#[tokio::test]
async fn mount_upload_streams_before_archive_finishes() {
    let mut data = vec![0u8; 2 * 1024 * 1024];
    rand::rng().fill(&mut data[..]);
    let expected = data.clone();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let app = Router::new().route(
        "/init-mounts",
        post(move |body: Body| {
            let expected = expected.clone();
            let release_tx = release_tx.clone();
            async move {
                let mut chunks = body.into_data_stream();
                let mut gzip = chunks.next().await.unwrap().unwrap().to_vec();
                release_tx.send(()).unwrap();
                while let Some(chunk) = chunks.next().await {
                    gzip.extend(chunk.unwrap());
                }
                let mut contents = Vec::new();
                GzDecoder::new(&gzip[..])
                    .read_to_end(&mut contents)
                    .unwrap();
                assert_eq!(contents, expected);
                "uploaded"
            }
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let _server = AbortOnDropHandle::new(tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    }));
    let client = PodClient::new_with_timeout(&url, "test", Duration::from_secs(10)).unwrap();
    client
        .init_mounts_async(move |writer| {
            // Staging the entire archive before sending would deadlock here.
            writer.write_all(&data[..1024 * 1024])?;
            writer.flush()?;
            release_rx.recv_timeout(Duration::from_secs(5))?;
            writer.write_all(&data[1024 * 1024..])?;
            Ok(())
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn mount_upload_cancellation_releases_blocked_producer() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let _server = AbortOnDropHandle::new(tokio::spawn(async move {
        let (connection, _) = listener.accept().await.unwrap();
        std::future::pending::<()>().await;
        drop(connection);
    }));
    let mut data = vec![0u8; 64 * 1024];
    rand::rng().fill(&mut data[..]);
    let (started_tx, started_rx) = oneshot::channel();
    let (finished_tx, finished_rx) = oneshot::channel();
    let client = PodClient::new_with_timeout(&url, "test", Duration::from_secs(30)).unwrap();
    let request = AbortOnDropHandle::new(tokio::spawn(async move {
        client
            .init_mounts_async(move |writer| {
                started_tx.send(()).unwrap();
                let result = (|| -> anyhow::Result<()> {
                    loop {
                        writer.write_all(&data)?;
                    }
                })();
                finished_tx.send(()).unwrap();
                result
            })
            .await
    }));
    timeout(Duration::from_secs(5), started_rx)
        .await
        .unwrap()
        .unwrap();
    // Keep the server alive without reading. Cancelling the request must
    // release its producer even if the network still owns the HTTP body.
    request.abort();
    assert!(request.await.unwrap_err().is_cancelled());
    timeout(Duration::from_secs(5), finished_rx)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn mount_upload_checks_body_after_tar_end() {
    let app = Router::new()
        .route("/init-mounts", post(super::init_mounts_handler))
        .layer(tower_http::decompression::RequestDecompressionLayer::new());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/init-mounts", listener.local_addr().unwrap());
    let _server = AbortOnDropHandle::new(tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    }));
    let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    gzip.write_all(&[0; 1024]).unwrap();
    let mut gzip = gzip.finish().unwrap();
    let mut trailer = gzip.split_off(gzip.len() - 8);
    trailer[0] ^= 1;
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Vec<u8>, std::io::Error>>(2);
    let response = AbortOnDropHandle::new(tokio::spawn(async move {
        reqwest::Client::new()
            .post(url)
            .header("Content-Encoding", "gzip")
            .body(reqwest::Body::wrap_stream(
                tokio_stream::wrappers::ReceiverStream::new(rx),
            ))
            .send()
            .await
            .unwrap()
    }));
    tx.send(Ok(gzip)).await.unwrap();
    // The tar is complete, but success must wait for HTTP EOF and verify
    // the gzip trailer. Deliver a corrupt trailer only after that boundary.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let trailer_sent = tx.send(Ok(trailer)).await;
    drop(tx);
    let response = timeout(Duration::from_secs(5), response)
        .await
        .unwrap()
        .unwrap();
    assert!(!response.status().is_success());
    trailer_sent.unwrap();
}

#[tokio::test]
async fn mount_upload_reports_archive_error() {
    let app = Router::new().route(
        "/init-mounts",
        post(|body: Body| async move {
            let mut chunks = body.into_data_stream();
            while let Some(chunk) = chunks.next().await {
                if chunk.is_err() {
                    return axum::http::StatusCode::BAD_REQUEST;
                }
            }
            axum::http::StatusCode::OK
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let _server = AbortOnDropHandle::new(tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    }));
    let client = PodClient::new_with_timeout(&url, "test", Duration::from_secs(5)).unwrap();
    let error = client
        .init_mounts_async(|writer| {
            writer.write_all(b"partial archive")?;
            writer.flush()?;
            Err(anyhow::anyhow!("source file became unreadable"))
        })
        .await
        .unwrap_err();
    assert!(format!("{error:#}").contains("source file became unreadable"));
}

#[tokio::test]
async fn mount_upload_rejection_releases_producer() {
    let app = Router::new().route(
        "/init-mounts",
        post(|| async {
            (
                axum::http::StatusCode::FORBIDDEN,
                axum::Json(serde_json::json!({"error": "upload rejected"})),
            )
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let _server = AbortOnDropHandle::new(tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    }));
    let mut data = vec![0u8; 64 * 1024];
    rand::rng().fill(&mut data[..]);
    let client = PodClient::new_with_timeout(&url, "test", Duration::from_secs(5)).unwrap();
    let error = timeout(
        Duration::from_secs(5),
        client.init_mounts_async(move |writer| loop {
            writer.write_all(&data)?;
        }),
    )
    .await
    .unwrap()
    .unwrap_err();
    assert!(format!("{error:#}").contains("upload rejected"));
}
