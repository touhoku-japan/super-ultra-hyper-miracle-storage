//! Multipart uploads, server-side copies, checksums and conditional requests over HTTP.

mod common;

use base64::{Engine, engine::general_purpose::STANDARD as B64};
use common::{Server, signed_with_payload_hash, xml_values};
use md5::{Digest, Md5};

const MIB: usize = 1024 * 1024;

fn data(seed: u8, n: usize) -> Vec<u8> {
    (0..n)
        .map(|i| (i as u8).wrapping_mul(13).wrapping_add(seed))
        .collect()
}

fn crc32_b64(data: &[u8]) -> String {
    // Bitwise CRC-32 (IEEE), independent of the server's implementation.
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    B64.encode((!crc).to_be_bytes())
}

#[tokio::test]
async fn multipart_upload_over_http() {
    let srv = Server::start(3).await;
    let (_, c) = srv.tenant("alice").await;
    srv.s3(&c, "PUT", "/mpu", &[], &[], vec![]).await;

    let resp = srv
        .s3(
            &c,
            "POST",
            "/mpu/big.bin",
            &[("uploads", "")],
            &[("content-type", "application/x-tar")],
            vec![],
        )
        .await;
    assert_eq!(resp.status, 200, "{}", resp.text());
    let upload_id = xml_values(&resp.text(), "UploadId").remove(0);

    let p1 = data(1, 5 * MIB);
    let p2 = data(2, 777);
    let mut etags = Vec::new();
    for (n, body) in [("1", &p1), ("2", &p2)] {
        let resp = srv
            .s3(
                &c,
                "PUT",
                "/mpu/big.bin",
                &[("partNumber", n), ("uploadId", &upload_id)],
                &[("x-amz-checksum-crc32", &crc32_b64(body))],
                body.clone(),
            )
            .await;
        assert_eq!(resp.status, 200, "{}", resp.text());
        assert_eq!(
            resp.header("x-amz-checksum-crc32"),
            Some(crc32_b64(body).as_str())
        );
        etags.push(resp.header("etag").unwrap().to_owned());
    }

    let resp = srv
        .s3(
            &c,
            "GET",
            "/mpu/big.bin",
            &[("uploadId", &upload_id)],
            &[],
            vec![],
        )
        .await;
    assert_eq!(xml_values(&resp.text(), "PartNumber"), vec!["1", "2"]);

    let resp = srv
        .s3(&c, "GET", "/mpu", &[("uploads", "")], &[], vec![])
        .await;
    assert_eq!(
        xml_values(&resp.text(), "UploadId"),
        vec![upload_id.clone()]
    );

    let complete = format!(
        "<CompleteMultipartUpload>\
         <Part><PartNumber>1</PartNumber><ETag>{}</ETag></Part>\
         <Part><PartNumber>2</PartNumber><ETag>{}</ETag></Part>\
         </CompleteMultipartUpload>",
        etags[0], etags[1]
    );
    let resp = srv
        .s3(
            &c,
            "POST",
            "/mpu/big.bin",
            &[("uploadId", &upload_id)],
            &[],
            complete.into_bytes(),
        )
        .await;
    assert_eq!(resp.status, 200, "{}", resp.text());
    let text = resp.text();
    let etag = xml_values(&text, "ETag").remove(0);
    assert!(
        etag.ends_with("-2\"") || etag.ends_with("-2&quot;"),
        "{text}"
    );

    let resp = srv.s3(&c, "GET", "/mpu/big.bin", &[], &[], vec![]).await;
    let mut expected = p1.clone();
    expected.extend_from_slice(&p2);
    assert_eq!(resp.body.len(), expected.len());
    assert!(resp.body.as_ref() == expected.as_slice());
    assert_eq!(resp.header("content-type"), Some("application/x-tar"));

    let resp = srv
        .s3(&c, "GET", "/mpu", &[("uploads", "")], &[], vec![])
        .await;
    assert!(xml_values(&resp.text(), "UploadId").is_empty());
}

#[tokio::test]
async fn abort_and_bad_completions() {
    let srv = Server::start(1).await;
    let (_, c) = srv.tenant("alice").await;
    srv.s3(&c, "PUT", "/mpu", &[], &[], vec![]).await;
    let resp = srv
        .s3(&c, "POST", "/mpu/k", &[("uploads", "")], &[], vec![])
        .await;
    let id = xml_values(&resp.text(), "UploadId").remove(0);
    srv.s3(
        &c,
        "PUT",
        "/mpu/k",
        &[("partNumber", "1"), ("uploadId", &id)],
        &[],
        b"x".to_vec(),
    )
    .await;

    let bad = "<CompleteMultipartUpload><Part><PartNumber>1</PartNumber>\
               <ETag>\"00000000000000000000000000000000\"</ETag></Part></CompleteMultipartUpload>";
    let resp = srv
        .s3(
            &c,
            "POST",
            "/mpu/k",
            &[("uploadId", &id)],
            &[],
            bad.as_bytes().to_vec(),
        )
        .await;
    assert_eq!(
        resp.error_code().as_deref(),
        Some("InvalidPart"),
        "{}",
        resp.text()
    );

    let resp = srv
        .s3(&c, "DELETE", "/mpu/k", &[("uploadId", &id)], &[], vec![])
        .await;
    assert_eq!(resp.status, 204);
    let resp = srv
        .s3(
            &c,
            "PUT",
            "/mpu/k",
            &[("partNumber", "1"), ("uploadId", &id)],
            &[],
            b"x".to_vec(),
        )
        .await;
    assert_eq!(resp.error_code().as_deref(), Some("NoSuchUpload"));
    let resp = srv
        .s3(
            &c,
            "PUT",
            "/mpu/k",
            &[("partNumber", "1"), ("uploadId", "not-a-uuid")],
            &[],
            b"x".to_vec(),
        )
        .await;
    assert_eq!(resp.error_code().as_deref(), Some("NoSuchUpload"));
}

#[tokio::test]
async fn copy_object_and_upload_part_copy() {
    let srv = Server::start(1).await;
    let (_, c) = srv.tenant("alice").await;
    let (_, other) = srv.tenant("bob").await;
    srv.s3(&c, "PUT", "/cpy", &[], &[], vec![]).await;
    srv.s3(&other, "PUT", "/bobs", &[], &[], vec![]).await;
    srv.s3(&other, "PUT", "/bobs/private", &[], &[], b"bob".to_vec())
        .await;
    let src = data(9, 6 * MIB);
    srv.s3(
        &c,
        "PUT",
        "/cpy/src",
        &[],
        &[("x-amz-meta-a", "1"), ("content-type", "text/x")],
        src.clone(),
    )
    .await;

    // Plain copy keeps metadata.
    let resp = srv
        .s3(
            &c,
            "PUT",
            "/cpy/dst",
            &[],
            &[("x-amz-copy-source", "/cpy/src")],
            vec![],
        )
        .await;
    assert_eq!(resp.status, 200, "{}", resp.text());
    let resp = srv.s3(&c, "HEAD", "/cpy/dst", &[], &[], vec![]).await;
    assert_eq!(resp.header("x-amz-meta-a"), Some("1"));
    assert_eq!(resp.header("content-type"), Some("text/x"));

    // REPLACE uses the request's metadata.
    srv.s3(
        &c,
        "PUT",
        "/cpy/dst2",
        &[],
        &[
            ("x-amz-copy-source", "cpy/src"),
            ("x-amz-metadata-directive", "REPLACE"),
            ("x-amz-meta-b", "2"),
        ],
        vec![],
    )
    .await;
    let resp = srv.s3(&c, "HEAD", "/cpy/dst2", &[], &[], vec![]).await;
    assert_eq!(resp.header("x-amz-meta-a"), None);
    assert_eq!(resp.header("x-amz-meta-b"), Some("2"));

    // Another tenant's object cannot be a copy source.
    let resp = srv
        .s3(
            &c,
            "PUT",
            "/cpy/stolen",
            &[],
            &[("x-amz-copy-source", "/bobs/private")],
            vec![],
        )
        .await;
    assert_eq!(resp.status, 403, "{}", resp.text());

    // Multipart copy in two ranges, as the AWS CLI does for large objects.
    let resp = srv
        .s3(
            &c,
            "POST",
            "/cpy/assembled",
            &[("uploads", "")],
            &[],
            vec![],
        )
        .await;
    let id = xml_values(&resp.text(), "UploadId").remove(0);
    let split = 5 * MIB;
    let ranges = [
        format!("bytes=0-{}", split - 1),
        format!("bytes={split}-{}", src.len() - 1),
    ];
    let mut etags = Vec::new();
    for (i, range) in ranges.iter().enumerate() {
        let n = (i + 1).to_string();
        let resp = srv
            .s3(
                &c,
                "PUT",
                "/cpy/assembled",
                &[("partNumber", &n), ("uploadId", &id)],
                &[
                    ("x-amz-copy-source", "/cpy/src"),
                    ("x-amz-copy-source-range", range),
                ],
                vec![],
            )
            .await;
        assert_eq!(resp.status, 200, "{}", resp.text());
        etags.push(xml_values(&resp.text(), "ETag").remove(0));
    }
    let complete = format!(
        "<CompleteMultipartUpload><Part><PartNumber>1</PartNumber><ETag>{}</ETag></Part>\
         <Part><PartNumber>2</PartNumber><ETag>{}</ETag></Part></CompleteMultipartUpload>",
        etags[0].replace("&quot;", "\""),
        etags[1].replace("&quot;", "\"")
    );
    let resp = srv
        .s3(
            &c,
            "POST",
            "/cpy/assembled",
            &[("uploadId", &id)],
            &[],
            complete.into_bytes(),
        )
        .await;
    assert_eq!(resp.status, 200, "{}", resp.text());
    let resp = srv.s3(&c, "GET", "/cpy/assembled", &[], &[], vec![]).await;
    assert!(resp.body.as_ref() == src.as_slice());
}

#[tokio::test]
async fn header_checksums_are_verified() {
    let srv = Server::start(1).await;
    let (_, c) = srv.tenant("alice").await;
    srv.s3(&c, "PUT", "/chk", &[], &[], vec![]).await;

    let resp = srv
        .s3(
            &c,
            "PUT",
            "/chk/ok",
            &[],
            &[("x-amz-checksum-crc32", &crc32_b64(b"hello"))],
            b"hello".to_vec(),
        )
        .await;
    assert_eq!(resp.status, 200, "{}", resp.text());

    let resp = srv
        .s3(
            &c,
            "PUT",
            "/chk/bad",
            &[],
            &[("x-amz-checksum-crc32", &crc32_b64(b"other"))],
            b"hello".to_vec(),
        )
        .await;
    assert_eq!(
        resp.error_code().as_deref(),
        Some("BadDigest"),
        "{}",
        resp.text()
    );
    assert_eq!(
        srv.s3(&c, "HEAD", "/chk/bad", &[], &[], vec![])
            .await
            .status,
        404
    );
}

/// An aws-chunked body with an unsigned payload and a trailing CRC32, the default upload format
/// of current AWS SDKs.
fn chunked_with_trailer(payload: &[u8], crc: &str) -> Vec<u8> {
    let mut body = Vec::new();
    for chunk in payload.chunks(64 * 1024) {
        body.extend_from_slice(format!("{:x}\r\n", chunk.len()).as_bytes());
        body.extend_from_slice(chunk);
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("0\r\nx-amz-checksum-crc32:{crc}\r\n\r\n").as_bytes());
    body
}

#[tokio::test]
async fn aws_chunked_trailer_checksums_are_verified() {
    let srv = Server::start(1).await;
    let (_, c) = srv.tenant("alice").await;
    srv.s3(&c, "PUT", "/chk", &[], &[], vec![]).await;
    let payload = data(5, 200_000);

    for (key, crc, ok) in [
        ("good", crc32_b64(&payload), true),
        ("bad", crc32_b64(b"x"), false),
    ] {
        let len = payload.len().to_string();
        let headers = [
            ("content-encoding", "aws-chunked"),
            ("x-amz-decoded-content-length", len.as_str()),
            ("x-amz-trailer", "x-amz-checksum-crc32"),
        ];
        let req = signed_with_payload_hash(
            &c,
            "PUT",
            &format!("/chk/{key}"),
            &[],
            &headers,
            chunked_with_trailer(&payload, &crc).into(),
            "STREAMING-UNSIGNED-PAYLOAD-TRAILER",
        );
        let resp = srv.send(req).await;
        if ok {
            assert_eq!(resp.status, 200, "{}", resp.text());
            let got = srv.s3(&c, "GET", "/chk/good", &[], &[], vec![]).await;
            assert!(got.body.as_ref() == payload.as_slice());
        } else {
            assert_eq!(
                resp.error_code().as_deref(),
                Some("BadDigest"),
                "{}",
                resp.text()
            );
            assert_eq!(
                srv.s3(&c, "HEAD", "/chk/bad", &[], &[], vec![])
                    .await
                    .status,
                404
            );
        }
    }
}

#[tokio::test]
async fn conditional_requests() {
    let srv = Server::start(1).await;
    let (_, c) = srv.tenant("alice").await;
    srv.s3(&c, "PUT", "/cnd", &[], &[], vec![]).await;
    srv.s3(&c, "PUT", "/cnd/k", &[], &[], b"v1".to_vec()).await;
    let etag = format!("\"{}\"", hex::encode(Md5::digest(b"v1")));

    let resp = srv
        .s3(
            &c,
            "GET",
            "/cnd/k",
            &[],
            &[("if-none-match", &etag)],
            vec![],
        )
        .await;
    assert_eq!(resp.status, 304);
    let resp = srv
        .s3(
            &c,
            "GET",
            "/cnd/k",
            &[],
            &[("if-match", "\"nope\"")],
            vec![],
        )
        .await;
    assert_eq!(resp.status, 412);
    let resp = srv
        .s3(&c, "GET", "/cnd/k", &[], &[("if-match", &etag)], vec![])
        .await;
    assert_eq!(resp.status, 200);
    let resp = srv
        .s3(
            &c,
            "HEAD",
            "/cnd/k",
            &[],
            &[("if-modified-since", "Sat, 01 Jan 2000 00:00:00 GMT")],
            vec![],
        )
        .await;
    assert_eq!(resp.status, 200);
    let resp = srv
        .s3(
            &c,
            "HEAD",
            "/cnd/k",
            &[],
            &[("if-modified-since", "Fri, 01 Jan 2100 00:00:00 GMT")],
            vec![],
        )
        .await;
    assert_eq!(resp.status, 304);

    // Conditional create.
    let resp = srv
        .s3(
            &c,
            "PUT",
            "/cnd/k",
            &[],
            &[("if-none-match", "*")],
            b"v2".to_vec(),
        )
        .await;
    assert_eq!(resp.status, 412);
    let resp = srv
        .s3(
            &c,
            "PUT",
            "/cnd/new",
            &[],
            &[("if-none-match", "*")],
            b"v".to_vec(),
        )
        .await;
    assert_eq!(resp.status, 200);
    let resp = srv.s3(&c, "GET", "/cnd/k", &[], &[], vec![]).await;
    assert_eq!(resp.body.as_ref(), b"v1");
    let _ = B64.encode(b"");
}
