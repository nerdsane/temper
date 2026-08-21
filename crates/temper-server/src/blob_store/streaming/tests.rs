use base64::Engine as _;
use bytes::Bytes;
use futures_util::{StreamExt as _, TryStreamExt as _, stream};
use sha2::Digest as _;

use super::{BlobByteStream, BlobObjectStream, decode_json_base64_stream};

fn object(chunks: &[&'static [u8]]) -> BlobObjectStream {
    let content_length = chunks.iter().map(|chunk| chunk.len() as u64).sum();
    let chunks = chunks
        .iter()
        .map(|chunk| Ok::<_, std::io::Error>(Bytes::from_static(chunk)))
        .collect::<Vec<_>>();
    let stream: BlobByteStream = Box::pin(stream::iter(chunks));
    BlobObjectStream {
        content_length,
        stream,
    }
}

#[tokio::test]
async fn json_base64_decoder_handles_arbitrary_chunk_boundaries() {
    let decoded = decode_json_base64_stream(object(&[b"\"Y", b"WJ", b"jZ", b"GVm", b"\""]), 6)
        .into_stream()
        .try_collect::<Vec<_>>()
        .await
        .expect("decode split base64")
        .concat();
    assert_eq!(decoded, b"abcdef");
}

#[tokio::test]
async fn json_base64_decoder_rejects_data_after_padding() {
    let error = decode_json_base64_stream(object(&[b"\"YQ==x\""]), 1)
        .into_stream()
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .find_map(Result::err)
        .expect("invalid stream must fail");
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
}

#[tokio::test]
async fn json_base64_decoder_rejects_decoded_length_mismatch() {
    let error = decode_json_base64_stream(object(&[b"\"YQ==\""]), 2)
        .into_stream()
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .find_map(Result::err)
        .expect("short decoded stream must fail");
    assert_eq!(error.kind(), std::io::ErrorKind::UnexpectedEof);
}

#[tokio::test]
async fn json_base64_decoder_coalesces_output_into_bounded_chunks() {
    let expected = vec![0x5au8; 128 * 1024];
    let encoded = serde_json::to_vec(&base64::engine::general_purpose::STANDARD.encode(&expected))
        .expect("serialize base64 string");
    let content_length = encoded.len() as u64;
    let source: BlobByteStream = Box::pin(stream::iter([Ok(Bytes::from(encoded))]));
    let chunks = decode_json_base64_stream(
        BlobObjectStream {
            content_length,
            stream: source,
        },
        expected.len() as u64,
    )
    .into_stream()
    .try_collect::<Vec<_>>()
    .await
    .expect("decode large base64 string");

    assert_eq!(
        chunks.len(),
        3,
        "decoder must not emit one chunk per quartet"
    );
    assert_eq!(chunks.concat(), expected);
}

#[tokio::test]
async fn content_addressed_stream_verifies_sha256() {
    let expected = format!("{:x}", sha2::Sha256::digest(b"abcdef"));
    let verified = object(&[b"abc", b"def"])
        .verify_sha256(&expected)
        .into_stream()
        .try_collect::<Vec<_>>()
        .await
        .expect("matching digest");
    assert_eq!(verified.concat(), b"abcdef");

    let error = object(&[b"tampered"])
        .verify_sha256(&expected)
        .into_stream()
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .find_map(Result::err)
        .expect("digest mismatch must fail");
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
}
