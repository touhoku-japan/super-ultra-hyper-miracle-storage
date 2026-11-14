//! Checksums, conditional writes, versioned copy sources, delimiters on version and upload
//! listings, and the remaining lifecycle actions, over HTTP.

mod common;

use base64::{Engine, engine::general_purpose::STANDARD as B64};
use common::{Server, xml_values};
use md5::{Digest, Md5};
use s3s::crypto::{Checksum as _, Crc64Nvme, Sha256};

const MIB: usize = 1024 * 1024;

fn md5_b64(s: &str) -> String {
    B64.encode(Md5::digest(s.as_bytes()))
}

fn sha256_b64(data: &[u8]) -> String {
    B64.encode(Sha256::checksum(data))
}

fn upload_id(xml: &str) -> String {
    xml_values(xml, "UploadId").remove(0)
}

#[tokio::test]
async fn checksums_are_stored_and_returned_on_request() {
    let srv = Server::start(1).await;
    let (_, c) = srv.tenant("alice").await;
    srv.s3(&c, "PUT", "/sums", &[], &[], vec![]).await;
    let sum = sha256_b64(b"hello");
    let resp = srv
        .s3(
            &c,
            "PUT",
            "/sums/a",
            &[],
            &[("x-amz-checksum-sha256", &sum)],
            b"hello".to_vec(),
        )
        .await;
    assert_eq!(resp.status, 200, "{}", resp.text());

    let mode = [("x-amz-checksum-mode", "ENABLED")];
    for method in ["GET", "HEAD"] {
        let resp = srv.s3(&c, method, "/sums/a", &[], &mode, vec![]).await;
        assert_eq!(resp.header("x-amz-checksum-sha256"), Some(sum.as_str()));
        assert_eq!(resp.header("x-amz-checksum-type"), Some("FULL_OBJECT"));
        // Without x-amz-checksum-mode the checksum is not returned (as in S3).
        let resp = srv.s3(&c, method, "/sums/a", &[], &[], vec![]).await;
        assert_eq!(resp.header("x-amz-checksum-sha256"), None);
    }
    // Ranged reads do not describe the whole object.
    let resp = srv
        .s3(
            &c,
            "GET",
            "/sums/a",
            &[],
            &[("x-amz-checksum-mode", "ENABLED"), ("range", "bytes=0-1")],
            vec![],
        )
        .await;
    assert_eq!(resp.header("x-amz-checksum-sha256"), None);

    // A copy recomputes the source's algorithm.
    let resp = srv
        .s3(
            &c,
            "PUT",
            "/sums/b",
            &[],
            &[("x-amz-copy-source", "/sums/a")],
            vec![],
        )
        .await;
    assert_eq!(
        xml_values(&resp.text(), "ChecksumSHA256"),
        vec![sum.clone()]
    );
    let resp = srv.s3(&c, "HEAD", "/sums/b", &[], &mode, vec![]).await;
    assert_eq!(resp.header("x-amz-checksum-sha256"), Some(sum.as_str()));
}

async fn multipart(srv: &Server, c: &common::Creds, key: &str, algo: &str, kind: &str) -> String {
    let resp = srv
        .s3(
            c,
            "POST",
            &format!("/mpu/{key}"),
            &[("uploads", "")],
            &[
                ("x-amz-checksum-algorithm", algo),
                ("x-amz-checksum-type", kind),
            ],
            vec![],
        )
        .await;
    assert_eq!(resp.status, 200, "{}", resp.text());
    let xml = resp.text();
    assert_eq!(resp.header("x-amz-checksum-algorithm"), Some(algo));
    assert_eq!(resp.header("x-amz-checksum-type"), Some(kind));
    upload_id(&xml)
}

#[tokio::test]
async fn multipart_full_object_crc_is_the_whole_object_crc() {
    let srv = Server::start(1).await;
    let (_, c) = srv.tenant("alice").await;
    srv.s3(&c, "PUT", "/mpu", &[], &[], vec![]).await;
    let id = multipart(&srv, &c, "big", "CRC64NVME", "FULL_OBJECT").await;
    let p1 = vec![7u8; 5 * MIB];
    let p2 = b"tail".to_vec();
    let mut etags = Vec::new();
    for (n, body) in [("1", &p1), ("2", &p2)] {
        // No client checksum: the server computes the upload's algorithm itself.
        let resp = srv
            .s3(
                &c,
                "PUT",
                "/mpu/big",
                &[("partNumber", n), ("uploadId", &id)],
                &[],
                body.clone(),
            )
            .await;
        assert_eq!(resp.status, 200, "{}", resp.text());
        assert!(resp.header("x-amz-checksum-crc64nvme").is_some());
        etags.push(resp.header("etag").unwrap().to_owned());
    }
    let parts = srv
        .s3(&c, "GET", "/mpu/big", &[("uploadId", &id)], &[], vec![])
        .await
        .text();
    assert_eq!(xml_values(&parts, "ChecksumCRC64NVME").len(), 2, "{parts}");

    let done = format!(
        "<CompleteMultipartUpload><Part><PartNumber>1</PartNumber><ETag>{}</ETag></Part>\
         <Part><PartNumber>2</PartNumber><ETag>{}</ETag></Part></CompleteMultipartUpload>",
        etags[0], etags[1]
    );
    let resp = srv
        .s3(&c, "POST", "/mpu/big", &[("uploadId", &id)], &[], done)
        .await;
    let xml = resp.text();
    let mut whole = p1.clone();
    whole.extend_from_slice(&p2);
    let expected = B64.encode(Crc64Nvme::checksum(&whole));
    assert_eq!(
        xml_values(&xml, "ChecksumCRC64NVME"),
        vec![expected.clone()],
        "{xml}"
    );
    assert_eq!(xml_values(&xml, "ChecksumType"), vec!["FULL_OBJECT"]);

    let resp = srv
        .s3(
            &c,
            "HEAD",
            "/mpu/big",
            &[],
            &[("x-amz-checksum-mode", "ENABLED")],
            vec![],
        )
        .await;
    assert_eq!(
        resp.header("x-amz-checksum-crc64nvme"),
        Some(expected.as_str())
    );
}

#[tokio::test]
async fn multipart_composite_checksum_and_part_verification() {
    let srv = Server::start(1).await;
    let (_, c) = srv.tenant("alice").await;
    srv.s3(&c, "PUT", "/mpu", &[], &[], vec![]).await;
    let id = multipart(&srv, &c, "obj", "SHA256", "COMPOSITE").await;
    let p1 = vec![1u8; 5 * MIB];
    let p2 = b"end".to_vec();
    let mut etags = Vec::new();
    for (n, body) in [("1", &p1), ("2", &p2)] {
        let resp = srv
            .s3(
                &c,
                "PUT",
                "/mpu/obj",
                &[("partNumber", n), ("uploadId", &id)],
                &[("x-amz-checksum-sha256", &sha256_b64(body))],
                body.clone(),
            )
            .await;
        assert_eq!(resp.status, 200, "{}", resp.text());
        etags.push(resp.header("etag").unwrap().to_owned());
    }
    // A part checksum of another algorithm than the upload's is rejected.
    let resp = srv
        .s3(
            &c,
            "PUT",
            "/mpu/obj",
            &[("partNumber", "3"), ("uploadId", &id)],
            &[("x-amz-checksum-crc32", "NhCmhg==")],
            b"hello".to_vec(),
        )
        .await;
    assert_eq!(resp.status, 400, "{}", resp.text());

    let complete = |sum2: &str| {
        format!(
            "<CompleteMultipartUpload><Part><PartNumber>1</PartNumber><ETag>{}</ETag>\
             <ChecksumSHA256>{}</ChecksumSHA256></Part>\
             <Part><PartNumber>2</PartNumber><ETag>{}</ETag><ChecksumSHA256>{sum2}</ChecksumSHA256>\
             </Part></CompleteMultipartUpload>",
            etags[0],
            sha256_b64(&p1),
            etags[1]
        )
    };
    let resp = srv
        .s3(
            &c,
            "POST",
            "/mpu/obj",
            &[("uploadId", &id)],
            &[],
            complete(&sha256_b64(b"wrong")),
        )
        .await;
    assert_eq!(
        resp.error_code().as_deref(),
        Some("InvalidPart"),
        "{}",
        resp.text()
    );

    let resp = srv
        .s3(
            &c,
            "POST",
            "/mpu/obj",
            &[("uploadId", &id)],
            &[],
            complete(&sha256_b64(&p2)),
        )
        .await;
    let xml = resp.text();
    let mut concat = Sha256::checksum(&p1).as_ref().to_vec();
    concat.extend_from_slice(Sha256::checksum(&p2).as_ref());
    let expected = format!("{}-2", B64.encode(Sha256::checksum(&concat)));
    assert_eq!(xml_values(&xml, "ChecksumSHA256"), vec![expected], "{xml}");
    assert_eq!(xml_values(&xml, "ChecksumType"), vec!["COMPOSITE"]);
}

#[tokio::test]
async fn invalid_multipart_checksum_settings_are_rejected() {
    let srv = Server::start(1).await;
    let (_, c) = srv.tenant("alice").await;
    srv.s3(&c, "PUT", "/mpu", &[], &[], vec![]).await;
    let resp = srv
        .s3(
            &c,
            "POST",
            "/mpu/x",
            &[("uploads", "")],
            &[
                ("x-amz-checksum-algorithm", "SHA256"),
                ("x-amz-checksum-type", "FULL_OBJECT"),
            ],
            vec![],
        )
        .await;
    assert_eq!(resp.status, 400, "{}", resp.text());
}

#[tokio::test]
async fn if_match_on_put() {
    let srv = Server::start(1).await;
    let (_, c) = srv.tenant("alice").await;
    srv.s3(&c, "PUT", "/cas", &[], &[], vec![]).await;
    let resp = srv.s3(&c, "PUT", "/cas/k", &[], &[], b"one".to_vec()).await;
    let etag = resp.header("etag").unwrap().to_owned();

    let resp = srv
        .s3(
            &c,
            "PUT",
            "/cas/k",
            &[],
            &[("if-match", "\"00000000000000000000000000000000\"")],
            b"two".to_vec(),
        )
        .await;
    assert_eq!(resp.status, 412);
    let resp = srv
        .s3(
            &c,
            "PUT",
            "/cas/missing",
            &[],
            &[("if-match", &etag)],
            b"two".to_vec(),
        )
        .await;
    assert_eq!(resp.status, 404);
    let resp = srv
        .s3(
            &c,
            "PUT",
            "/cas/k",
            &[],
            &[("if-match", &etag)],
            b"two".to_vec(),
        )
        .await;
    assert_eq!(resp.status, 200, "{}", resp.text());
    let resp = srv.s3(&c, "GET", "/cas/k", &[], &[], vec![]).await;
    assert_eq!(resp.body.as_ref(), b"two");
}

#[tokio::test]
async fn copy_from_an_old_version() {
    let srv = Server::start(1).await;
    let (_, c) = srv.tenant("alice").await;
    srv.s3(&c, "PUT", "/ver", &[], &[], vec![]).await;
    let cfg = "<VersioningConfiguration><Status>Enabled</Status></VersioningConfiguration>";
    srv.s3(
        &c,
        "PUT",
        "/ver",
        &[("versioning", "")],
        &[],
        cfg.as_bytes().to_vec(),
    )
    .await;
    let v1 = srv
        .s3(&c, "PUT", "/ver/doc", &[], &[], b"first".to_vec())
        .await
        .header("x-amz-version-id")
        .unwrap()
        .to_owned();
    srv.s3(&c, "PUT", "/ver/doc", &[], &[], b"second".to_vec())
        .await;

    let source = format!("/ver/doc?versionId={v1}");
    let resp = srv
        .s3(
            &c,
            "PUT",
            "/ver/restored",
            &[],
            &[("x-amz-copy-source", &source)],
            vec![],
        )
        .await;
    assert_eq!(resp.status, 200, "{}", resp.text());
    assert_eq!(
        resp.header("x-amz-copy-source-version-id"),
        Some(v1.as_str())
    );
    assert!(resp.header("x-amz-version-id").is_some());
    let resp = srv.s3(&c, "GET", "/ver/restored", &[], &[], vec![]).await;
    assert_eq!(resp.body.as_ref(), b"first");
}

#[tokio::test]
async fn delimiter_on_version_and_upload_listings() {
    let srv = Server::start(1).await;
    let (_, c) = srv.tenant("alice").await;
    srv.s3(&c, "PUT", "/dlm", &[], &[], vec![]).await;
    for k in ["photos/a.jpg", "photos/b.jpg", "readme"] {
        srv.s3(&c, "PUT", &format!("/dlm/{k}"), &[], &[], b"x".to_vec())
            .await;
        srv.s3(
            &c,
            "POST",
            &format!("/dlm/{k}"),
            &[("uploads", "")],
            &[],
            vec![],
        )
        .await;
    }
    let xml = srv
        .s3(
            &c,
            "GET",
            "/dlm",
            &[("versions", ""), ("delimiter", "/")],
            &[],
            vec![],
        )
        .await
        .text();
    assert!(
        xml.contains("<CommonPrefixes><Prefix>photos/</Prefix></CommonPrefixes>"),
        "{xml}"
    );
    assert_eq!(xml_values(&xml, "Key"), vec!["readme"]);
    assert_eq!(xml_values(&xml, "Delimiter"), vec!["/"]);

    let xml = srv
        .s3(
            &c,
            "GET",
            "/dlm",
            &[("uploads", ""), ("delimiter", "/")],
            &[],
            vec![],
        )
        .await
        .text();
    assert!(
        xml.contains("<CommonPrefixes><Prefix>photos/</Prefix></CommonPrefixes>"),
        "{xml}"
    );
    assert_eq!(xml_values(&xml, "Key"), vec!["readme"]);
}

#[tokio::test]
async fn lifecycle_transitions_and_delete_marker_rules_round_trip() {
    let srv = Server::start(3).await;
    let (_, c) = srv.tenant("alice").await;
    srv.s3(&c, "PUT", "/tier", &[], &[], vec![]).await;
    let put = |cfg: &str| {
        let cfg = cfg.to_owned();
        let srv = &srv;
        let c = &c;
        async move {
            srv.s3(
                c,
                "PUT",
                "/tier",
                &[("lifecycle", "")],
                &[("content-md5", &md5_b64(&cfg))],
                cfg.into_bytes(),
            )
            .await
        }
    };
    let bad = "<LifecycleConfiguration><Rule><ID>t</ID><Filter><Prefix></Prefix></Filter>\
               <Status>Enabled</Status><Transition><Days>30</Days>\
               <StorageClass>GLACIER</StorageClass></Transition></Rule></LifecycleConfiguration>";
    let resp = put(bad).await;
    assert_eq!(
        resp.error_code().as_deref(),
        Some("InvalidStorageClass"),
        "{}",
        resp.text()
    );

    let cfg = "<LifecycleConfiguration><Rule><ID>t</ID><Filter><Prefix>logs/</Prefix></Filter>\
               <Status>Enabled</Status><Transition><Days>30</Days>\
               <StorageClass>STANDARD_IA</StorageClass></Transition>\
               <NoncurrentVersionTransition><NoncurrentDays>7</NoncurrentDays>\
               <StorageClass>REDUCED_REDUNDANCY</StorageClass></NoncurrentVersionTransition>\
               <Expiration><ExpiredObjectDeleteMarker>true</ExpiredObjectDeleteMarker></Expiration>\
               </Rule></LifecycleConfiguration>";
    let resp = put(cfg).await;
    assert_eq!(resp.status, 200, "{}", resp.text());
    let xml = srv
        .s3(&c, "GET", "/tier", &[("lifecycle", "")], &[], vec![])
        .await
        .text();
    let mut classes = xml_values(&xml, "StorageClass");
    classes.sort();
    assert_eq!(classes, vec!["REDUCED_REDUNDANCY", "STANDARD_IA"], "{xml}");
    assert_eq!(xml_values(&xml, "Days"), vec!["30"]);
    assert_eq!(xml_values(&xml, "NoncurrentDays"), vec!["7"]);
    assert_eq!(xml_values(&xml, "ExpiredObjectDeleteMarker"), vec!["true"]);
}

#[tokio::test]
async fn uploads_without_a_checksum_get_crc64nvme() {
    let srv = Server::start(1).await;
    let (_, c) = srv.tenant("alice").await;
    srv.s3(&c, "PUT", "/dflt", &[], &[], vec![]).await;
    let resp = srv
        .s3(&c, "PUT", "/dflt/a", &[], &[], b"hello".to_vec())
        .await;
    let crc = B64.encode(Crc64Nvme::checksum(b"hello"));
    assert_eq!(resp.header("x-amz-checksum-crc64nvme"), Some(crc.as_str()));
    let resp = srv
        .s3(
            &c,
            "HEAD",
            "/dflt/a",
            &[],
            &[("x-amz-checksum-mode", "ENABLED")],
            vec![],
        )
        .await;
    assert_eq!(resp.header("x-amz-checksum-crc64nvme"), Some(crc.as_str()));

    // A multipart upload without an algorithm gets the full-object CRC64NVME too.
    let resp = srv
        .s3(&c, "POST", "/dflt/mp", &[("uploads", "")], &[], vec![])
        .await;
    let id = upload_id(&resp.text());
    let p1 = vec![3u8; 5 * MIB];
    let mut etags = Vec::new();
    for (n, body) in [("1", p1.clone()), ("2", b"end".to_vec())] {
        let resp = srv
            .s3(
                &c,
                "PUT",
                "/dflt/mp",
                &[("partNumber", n), ("uploadId", &id)],
                &[],
                body,
            )
            .await;
        etags.push(resp.header("etag").unwrap().to_owned());
    }
    let done = format!(
        "<CompleteMultipartUpload><Part><PartNumber>1</PartNumber><ETag>{}</ETag></Part>\
         <Part><PartNumber>2</PartNumber><ETag>{}</ETag></Part></CompleteMultipartUpload>",
        etags[0], etags[1]
    );
    let xml = srv
        .s3(&c, "POST", "/dflt/mp", &[("uploadId", &id)], &[], done)
        .await
        .text();
    let mut whole = p1;
    whole.extend_from_slice(b"end");
    assert_eq!(
        xml_values(&xml, "ChecksumCRC64NVME"),
        vec![B64.encode(Crc64Nvme::checksum(&whole))],
        "{xml}"
    );
}

#[tokio::test]
async fn newer_checksum_algorithms_over_http() {
    use s3s::crypto::{Md5 as AmzMd5, Sha512, XxHash64};
    let srv = Server::start(1).await;
    let (_, c) = srv.tenant("alice").await;
    srv.s3(&c, "PUT", "/algo", &[], &[], vec![]).await;
    for (header, sum) in [
        (
            "x-amz-checksum-sha512",
            B64.encode(Sha512::checksum(b"data")),
        ),
        ("x-amz-checksum-md5", B64.encode(AmzMd5::checksum(b"data"))),
        (
            "x-amz-checksum-xxhash64",
            B64.encode(XxHash64::checksum(b"data")),
        ),
    ] {
        let resp = srv
            .s3(
                &c,
                "PUT",
                "/algo/k",
                &[],
                &[(header, &sum)],
                b"data".to_vec(),
            )
            .await;
        assert_eq!(resp.status, 200, "{header}: {}", resp.text());
        let resp = srv
            .s3(
                &c,
                "HEAD",
                "/algo/k",
                &[],
                &[("x-amz-checksum-mode", "ENABLED")],
                vec![],
            )
            .await;
        assert_eq!(resp.header(header), Some(sum.as_str()));
        let resp = srv
            .s3(
                &c,
                "PUT",
                "/algo/k",
                &[],
                &[(header, "AAAA")],
                b"data".to_vec(),
            )
            .await;
        assert_eq!(resp.status, 400, "{header}");
    }
}

#[tokio::test]
async fn lifecycle_dates_sizes_and_newer_versions_round_trip() {
    let srv = Server::start(1).await;
    let (_, c) = srv.tenant("alice").await;
    srv.s3(&c, "PUT", "/lcx", &[], &[], vec![]).await;
    let put = |cfg: String| {
        let (srv, c) = (&srv, &c);
        async move {
            srv.s3(
                c,
                "PUT",
                "/lcx",
                &[("lifecycle", "")],
                &[("content-md5", &md5_b64(&cfg))],
                cfg.into_bytes(),
            )
            .await
        }
    };
    let cfg = "<LifecycleConfiguration><Rule><ID>r</ID><Filter><And><Prefix>logs/</Prefix>\
               <ObjectSizeGreaterThan>1024</ObjectSizeGreaterThan>\
               <ObjectSizeLessThan>1048576</ObjectSizeLessThan></And></Filter>\
               <Status>Enabled</Status><Expiration><Date>2030-01-01T00:00:00Z</Date></Expiration>\
               <NoncurrentVersionExpiration><NoncurrentDays>30</NoncurrentDays>\
               <NewerNoncurrentVersions>3</NewerNoncurrentVersions></NoncurrentVersionExpiration>\
               </Rule></LifecycleConfiguration>";
    let resp = put(cfg.to_owned()).await;
    assert_eq!(resp.status, 200, "{}", resp.text());
    let xml = srv
        .s3(&c, "GET", "/lcx", &[("lifecycle", "")], &[], vec![])
        .await
        .text();
    assert_eq!(xml_values(&xml, "Prefix"), vec!["logs/"], "{xml}");
    assert_eq!(xml_values(&xml, "ObjectSizeGreaterThan"), vec!["1024"]);
    assert_eq!(xml_values(&xml, "ObjectSizeLessThan"), vec!["1048576"]);
    assert_eq!(xml_values(&xml, "NewerNoncurrentVersions"), vec!["3"]);
    assert_eq!(xml_values(&xml, "Date").len(), 1, "{xml}");
    assert!(xml_values(&xml, "Date")[0].starts_with("2030-01-01T00:00:00"));

    // Tag filters are not supported (no object tagging).
    let tagged = "<LifecycleConfiguration><Rule><ID>t</ID><Filter><Tag><Key>a</Key><Value>b</Value>\
                  </Tag></Filter><Status>Enabled</Status><Expiration><Days>1</Days></Expiration>\
                  </Rule></LifecycleConfiguration>";
    assert_eq!(put(tagged.to_owned()).await.status, 501);
}
