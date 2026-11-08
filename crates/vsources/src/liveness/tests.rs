use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use super::*;

#[derive(Default)]
struct ScriptedFetcher {
    responses: Mutex<BTreeMap<String, ProbeResponse>>,
    requests: Mutex<Vec<FetchRequest>>,
    delay: Duration,
    active: AtomicUsize,
    peak: AtomicUsize,
}

fn url(s: &str) -> Url {
    Url::parse(s).unwrap_or_else(|e| panic!("test URL: {e}"))
}

fn response(path: &str, status: u16, ct: &str, body: &[u8]) -> ProbeResponse {
    ProbeResponse {
        url: url(&format!("https://cdn.example{path}")),
        status,
        headers: BTreeMap::from([("content-type".into(), ct.into())]),
        body: body.to_vec(),
        truncated: false,
    }
}

fn stream(path: &str, format: Format) -> Stream {
    Stream::new(url(&format!("https://cdn.example{path}")), format)
}

impl ScriptedFetcher {
    fn reply(&self, path: &str, response: ProbeResponse) {
        self.responses
            .lock()
            .unwrap_or_else(|e| panic!("lock: {e}"))
            .insert(path.into(), response);
    }
    fn requests(&self) -> Vec<FetchRequest> {
        self.requests
            .lock()
            .unwrap_or_else(|e| panic!("lock: {e}"))
            .clone()
    }
}

#[async_trait]
impl Fetcher for ScriptedFetcher {
    async fn request(&self, _: FetchRequest) -> Result<FetchResponse, FetchError> {
        panic!("media probes must use the bounded binary API")
    }
    async fn probe(
        &self,
        request: FetchRequest,
        limit: usize,
    ) -> Result<Option<ProbeResponse>, FetchError> {
        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(active, Ordering::SeqCst);
        tokio::time::sleep(self.delay).await;
        self.active.fetch_sub(1, Ordering::SeqCst);
        let path = request.url.path().to_string();
        self.requests
            .lock()
            .unwrap_or_else(|e| panic!("lock: {e}"))
            .push(request);
        let mut response = self
            .responses
            .lock()
            .unwrap_or_else(|e| panic!("lock: {e}"))
            .get(&path)
            .cloned();
        if let Some(response) = response.as_mut() {
            response.truncated |= response.body.len() >= limit;
            response.body.truncate(limit);
        }
        Ok(response)
    }
}

fn probe() -> StreamProbe {
    StreamProbe::new(ProbeConfig::default())
}

#[tokio::test]
async fn follows_redirected_master_child_and_segment_with_playback_headers() {
    let fetcher = ScriptedFetcher::default();
    let mut master = response(
        "/entry",
        200,
        "text/html",
        b"#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=99\n720/index.m3u8\n",
    );
    master.url = url("https://cdn.example/redirect/master.m3u8");
    fetcher.reply("/entry", master);
    fetcher.reply(
        "/redirect/720/index.m3u8",
        response(
            "/redirect/720/index.m3u8",
            200,
            "application/x-mpegurl",
            b"#EXTM3U\n#EXTINF:6,\nfirst.ts\n",
        ),
    );
    let mut ts = vec![0; 189];
    ts[0] = 0x47;
    ts[188] = 0x47;
    fetcher.reply(
        "/redirect/720/first.ts",
        response("/redirect/720/first.ts", 206, "image/jpeg", &ts),
    );
    let mut card = stream("/entry", Format::Hls);
    card.meta
        .request_headers
        .insert("Referer".into(), "https://origin.example/".into());
    card.meta
        .request_headers
        .insert("Origin".into(), "https://origin.example".into());
    card.meta
        .request_headers
        .insert("Range".into(), "bytes=999-".into());
    assert_eq!(probe().check(&fetcher, &card).await, Verdict::Alive);
    let requests = fetcher.requests();
    assert_eq!(requests.len(), 3);
    assert!(!requests[0].headers.contains_key("range"));
    assert_eq!(
        requests[2].headers.get("range").map(String::as_str),
        Some("bytes=0-2047")
    );
    for request in requests {
        assert_eq!(
            request.headers.get("referer").map(String::as_str),
            Some("https://origin.example/")
        );
        assert_eq!(
            request.headers.get("origin").map(String::as_str),
            Some("https://origin.example")
        );
    }
}

#[tokio::test]
async fn dead_segments_drop_live_masters() {
    for (status, ct, body) in [
        (404, "text/plain", "gone"),
        (200, "video/mp2t", "<html>error</html>"),
    ] {
        let fetcher = ScriptedFetcher::default();
        fetcher.reply(
            "/master.m3u8",
            response(
                "/master.m3u8",
                200,
                "text/plain",
                b"#EXTM3U\n#EXTINF:1,\npart.ts\n",
            ),
        );
        fetcher.reply(
            "/part.ts",
            response("/part.ts", status, ct, body.as_bytes()),
        );
        assert_eq!(
            probe()
                .check(&fetcher, &stream("/master.m3u8", Format::Hls))
                .await,
            Verdict::Dead
        );
    }
}

#[tokio::test]
async fn direct_files_reject_archives_html_json_and_thumbnails() {
    let cases = [
        (
            "application/octet-stream",
            b"PK\x03\x04\0archive".as_slice(),
        ),
        ("application/zip", b"P"),
        ("video/mp4", b"<html>not a movie</html>"),
        ("text/html", b"a login page"),
        ("application/json", b"{\"error\":\"not found\"}"),
        ("video/mp4", b"\x89PNG\r\n\x1a\n\0"),
    ];
    for (ct, bytes) in cases {
        let fetcher = ScriptedFetcher::default();
        fetcher.reply("/file", response("/file", 200, ct, bytes));
        assert_eq!(
            probe().check(&fetcher, &stream("/file", Format::Mp4)).await,
            Verdict::Dead,
            "{ct}"
        );
    }
}

#[tokio::test]
async fn filename_archive_and_real_media_with_lying_mime_types() {
    let fetcher = ScriptedFetcher::default();
    let mut zip = response("/zip", 206, "application/octet-stream", b"P");
    zip.headers.insert(
        "content-disposition".into(),
        "attachment; filename*=UTF-8''Some%20Show%2Ezip".into(),
    );
    fetcher.reply("/zip", zip);
    fetcher.reply(
        "/movie",
        response("/movie", 206, "text/html", b"\0\0\0\x18ftypisom"),
    );
    assert_eq!(
        probe().check(&fetcher, &stream("/zip", Format::Mp4)).await,
        Verdict::Dead
    );
    assert_eq!(
        probe()
            .check(&fetcher, &stream("/movie", Format::Mp4))
            .await,
        Verdict::Alive
    );
}

#[tokio::test]
async fn blocking_and_transient_statuses_are_never_dead() {
    let fetcher = ScriptedFetcher::default();
    for status in [401, 403, 429, 500, 502, 503] {
        fetcher.reply(
            "/file",
            response("/file", status, "text/html", b"<html>blocked</html>"),
        );
        assert_eq!(
            probe().check(&fetcher, &stream("/file", Format::Mp4)).await,
            Verdict::Unknown
        );
    }
    let mut cf = response("/file", 200, "text/html", b"<html>challenge</html>");
    cf.headers.insert("cf-mitigated".into(), "challenge".into());
    fetcher.reply("/file", cf);
    assert_eq!(
        probe().check(&fetcher, &stream("/file", Format::Mp4)).await,
        Verdict::Unknown
    );
}

#[tokio::test]
async fn coalesces_identical_probes_but_separates_playback_headers() {
    let fetcher = ScriptedFetcher {
        delay: Duration::from_millis(10),
        ..Default::default()
    };
    fetcher.reply("/file", response("/file", 404, "text/html", b"missing"));
    let gate = probe();
    let card = stream("/file", Format::Mp4);
    let (a, b) = tokio::join!(gate.check(&fetcher, &card), gate.check(&fetcher, &card));
    assert_eq!((a, b), (Verdict::Dead, Verdict::Dead));
    assert_eq!(fetcher.requests().len(), 1);
    let mut other = card.clone();
    other
        .meta
        .request_headers
        .insert("Referer".into(), "https://different.example/".into());
    gate.check(&fetcher, &other).await;
    assert_eq!(fetcher.requests().len(), 2);
    other.meta.request_headers =
        BTreeMap::from([("referer".into(), "https://different.example/".into())]);
    gate.check(&fetcher, &other).await;
    assert_eq!(fetcher.requests().len(), 2);
}

#[tokio::test]
async fn dead_verdicts_expire_and_do_not_poison_other_files_on_host() {
    let fetcher = ScriptedFetcher::default();
    fetcher.reply("/file", response("/file", 404, "text/plain", b"gone"));
    fetcher.reply(
        "/other",
        response("/other", 206, "video/mp4", b"\0\0\0\x18ftypisom"),
    );
    let gate = StreamProbe::new(ProbeConfig {
        dead_ttl: Duration::from_millis(10),
        ..Default::default()
    });
    let card = stream("/file", Format::Mp4);
    assert_eq!(gate.check(&fetcher, &card).await, Verdict::Dead);
    assert_eq!(
        gate.check(&fetcher, &stream("/other", Format::Mp4)).await,
        Verdict::Alive
    );
    fetcher.reply(
        "/file",
        response("/file", 206, "video/mp4", b"\0\0\0\x18ftypisom"),
    );
    tokio::time::sleep(Duration::from_millis(25)).await;
    assert_eq!(gate.check(&fetcher, &card).await, Verdict::Alive);
}

#[tokio::test]
async fn external_and_single_use_links_are_not_consumed() {
    let fetcher = ScriptedFetcher::default();
    let mut external = stream("/embed", Format::Unknown);
    external.is_external = true;
    let single_use = Stream::new(
        url("https://video-downloads.googleusercontent.com/token"),
        Format::Mp4,
    );
    let gate = probe();
    assert_eq!(gate.check(&fetcher, &external).await, Verdict::Unknown);
    assert_eq!(gate.check(&fetcher, &single_use).await, Verdict::Unknown);
    assert!(fetcher.requests().is_empty());
}

#[tokio::test]
async fn concurrency_and_whole_batch_deadline_are_bounded() {
    let fetcher = Arc::new(ScriptedFetcher {
        delay: Duration::from_millis(20),
        ..Default::default()
    });
    let gate = StreamProbe::new(ProbeConfig {
        concurrency: 2,
        timeout: Duration::from_millis(70),
        ..Default::default()
    });
    let cards: Vec<_> = (0..100)
        .map(|i| stream(&format!("/{i}"), Format::Mp4))
        .collect();
    let start = Instant::now();
    let result = gate.filter(fetcher.as_ref(), cards).await;
    assert_eq!(
        result.len(),
        100,
        "unprobed and inconclusive cards must ship"
    );
    assert!(start.elapsed() < Duration::from_millis(300));
    assert!(fetcher.peak.load(Ordering::SeqCst) <= 2);
    assert!(fetcher.requests().len() < 10);
}

#[tokio::test]
async fn byterange_playlists_probe_the_correct_file_offset() {
    let fetcher = ScriptedFetcher::default();
    fetcher.reply(
        "/master.m3u8",
        response(
            "/master.m3u8",
            200,
            "text/plain",
            b"#EXTM3U\n#EXT-X-BYTERANGE:999@12000\n#EXTINF:1,\nfile.mp4\n",
        ),
    );
    fetcher.reply(
        "/file.mp4",
        response("/file.mp4", 206, "video/mp4", b"\0\0\0\x18moofdata"),
    );
    assert_eq!(
        probe()
            .check(&fetcher, &stream("/master.m3u8", Format::Hls))
            .await,
        Verdict::Alive
    );
    assert_eq!(
        fetcher.requests()[1]
            .headers
            .get("range")
            .map(String::as_str),
        Some("bytes=12000-14047")
    );
}

#[tokio::test]
async fn empty_live_and_cyclic_playlists_remain_inconclusive() {
    for body in ["#EXTM3U\n", "#EXTM3U\nmaster.m3u8\n"] {
        let fetcher = ScriptedFetcher::default();
        fetcher.reply(
            "/master.m3u8",
            response("/master.m3u8", 200, "text/plain", body.as_bytes()),
        );
        assert_eq!(
            probe()
                .check(&fetcher, &stream("/master.m3u8", Format::Hls))
                .await,
            Verdict::Unknown
        );
    }
}

#[tokio::test]
async fn malformed_or_truncated_playlist_uris_and_dash_are_inconclusive() {
    for bytes in [
        b"#EXTM3U\n#EXT-X-BYTERANGE:12\nfile.ts\n#EXT-X-ENDLIST\n".as_slice(),
        b"#EXTM3U\n#EXT-X-BYTERANGE:12@18446744073709551615\nfile.ts\n#EXT-X-ENDLIST\n",
        b"<?xml version=\"1.0\"?><MPD xmlns=\"urn:mpeg:dash:schema:mpd:2011\"></MPD>",
    ] {
        let fetcher = ScriptedFetcher::default();
        fetcher.reply(
            "/master",
            response("/master", 200, "application/octet-stream", bytes),
        );
        assert_eq!(
            probe()
                .check(&fetcher, &stream("/master", Format::Hls))
                .await,
            Verdict::Unknown
        );
    }
    let fetcher = ScriptedFetcher::default();
    let mut truncated = response(
        "/master",
        200,
        "text/plain",
        b"#EXTM3U\n#EXTINF:6,\nhttps://incomplete",
    );
    truncated.truncated = true;
    fetcher.reply("/master", truncated);
    assert_eq!(
        probe()
            .check(&fetcher, &stream("/master", Format::Hls))
            .await,
        Verdict::Unknown
    );
    assert_eq!(fetcher.requests().len(), 1);
}

#[tokio::test]
async fn cancellation_releases_coalesced_work_and_permits() {
    let fetcher = ScriptedFetcher {
        delay: Duration::from_millis(40),
        ..Default::default()
    };
    fetcher.reply(
        "/file",
        response("/file", 206, "video/mp4", b"\0\0\0\x18ftypisom"),
    );
    let gate = StreamProbe::new(ProbeConfig {
        concurrency: 1,
        ..Default::default()
    });
    let card = stream("/file", Format::Mp4);
    assert!(
        tokio::time::timeout(Duration::from_millis(5), gate.check(&fetcher, &card))
            .await
            .is_err()
    );
    let verdict = tokio::time::timeout(Duration::from_secs(1), gate.check(&fetcher, &card)).await;
    assert_eq!(verdict.ok(), Some(Verdict::Alive));
}

#[test]
fn a_known_error_video_is_dead_even_with_valid_transport_stream_headers() {
    let mut bytes = vec![0; 2048];
    bytes[0] = 0x47;
    bytes[188] = 0x47;
    let payload = response("/same-host/opaque.ts", 200, "video/mp2t", &bytes);
    assert!(matches!(
        classify_payload(&payload, true),
        Payload::Verdict(Verdict::Dead)
    ));
    assert!(matches!(
        classify_payload(&payload, false),
        Payload::Verdict(Verdict::Alive)
    ));
    // A failed probe is not evidence that another player's IP cannot play it.
    let denied = response("/same-host/opaque.ts", 403, "video/mp2t", &bytes);
    assert!(matches!(
        classify_payload(&denied, true),
        Payload::Verdict(Verdict::Unknown)
    ));
}

#[tokio::test]
async fn progressive_validation_publishes_ready_media_before_a_stalled_alternative() {
    struct Uneven;
    #[async_trait]
    impl Fetcher for Uneven {
        async fn request(&self,_:FetchRequest)->Result<FetchResponse,FetchError>{panic!("bounded probes only")}
        async fn probe(&self,request:FetchRequest,_:usize)->Result<Option<ProbeResponse>,FetchError>{
            if request.url.path()=="/slow.mp4"{tokio::time::sleep(Duration::from_secs(30)).await;}
            Ok(Some(response(request.url.path(),206,"video/mp4",b"\0\0\0\x18ftypisom")))
        }
    }
    let gate=probe();let(tx,mut rx)=tokio::sync::mpsc::unbounded_channel();
    let run=gate.filter_progressive(&Uneven,vec![stream("/fast.mp4",Format::Mp4),stream("/slow.mp4",Format::Mp4)],None,|batch|{let _=tx.send(batch);});tokio::pin!(run);
    let first=tokio::time::timeout(Duration::from_secs(1),async{tokio::select!{batch=rx.recv()=>batch.unwrap(),_=&mut run=>panic!("must publish before whole batch completes")}}).await.unwrap();assert_eq!(first[0].url.path(),"/fast.mp4");
}

#[tokio::test]
async fn preferred_validation_inspects_explicit_audio_renditions_without_confusing_subtitles() {
    let fetcher=ScriptedFetcher::default();fetcher.reply("/multi.m3u8",response("/multi.m3u8",200,"application/vnd.apple.mpegurl",b"#EXTM3U\n#EXT-X-MEDIA:TYPE=AUDIO,LANGUAGE=\"fr\",URI=\"fr.m3u8\"\n#EXT-X-MEDIA:TYPE=AUDIO,LANGUAGE=\"eng\",URI=\"en.m3u8\"\n#EXT-X-MEDIA:TYPE=SUBTITLES,LANGUAGE=\"ja\",URI=\"ja.m3u8\"\n"));
    let output=probe().filter_progressive(&fetcher,vec![stream("/multi.m3u8",Format::Hls)],Some(vsources_core::types::CountryCode::En),|_|{}).await;
    assert_eq!(output[0].meta.languages,vec![vsources_core::types::CountryCode::Fr,vsources_core::types::CountryCode::En]);
}

#[tokio::test]
async fn unspecified_multi_audio_is_inspected_in_disguised_transport_stream_segments() {
    let mut section=vec![2,0xb0,0,0,1,0xc1,0,0,0xe1,0,0xf0,0];section.extend([27,0xe1,0,0xf0,0]);
    for(pid,language)in[(1,b"hin"),(2,b"eng")]{section.extend([15,0xe1,pid,0xf0,6,10,4]);section.extend(language);section.push(0);}section.extend([0;4]);section[2]=(section.len()-3)as u8;let mut packet=vec![0x47,0x41,0,0x10,0];packet.extend(section);packet.resize(188,0xff);let mut bytes=packet.clone();bytes.extend(packet);
    let fetcher=ScriptedFetcher::default();fetcher.reply("/opaque.m3u8",response("/opaque.m3u8",200,"application/vnd.apple.mpegurl",b"#EXTM3U\n#EXTINF:2,\nimage.jpg\n#EXT-X-ENDLIST\n"));fetcher.reply("/image.jpg",response("/image.jpg",206,"image/jpeg",&bytes));
    let mut source=stream("/opaque.m3u8",Format::Hls);source.meta.languages=vec![vsources_core::types::CountryCode::Multi];
    let output=probe().filter_progressive(&fetcher,vec![source],Some(vsources_core::types::CountryCode::En),|_|{}).await;assert!(output[0].meta.languages.contains(&vsources_core::types::CountryCode::En));assert!(output[0].meta.languages.contains(&vsources_core::types::CountryCode::Hi));
}
