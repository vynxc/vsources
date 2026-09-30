//! Generic fallback for direct URLs in supported embed pages and player configs.

use std::collections::HashSet;
use std::sync::LazyLock;
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use fancy_regex::Regex;
use url::Url;
use vsources_core::error::ExtractorError;
use vsources_core::traits::{Extractor, ResolveCtx};
use vsources_core::types::Stream;

use crate::helpers::{direct_stream, fetch_page, format_for_url};

const HOSTS: &[&str] = &[
    "vidsrc.to",
    "vidsrc.me",
    "vidsrc-embed.ru",
    "player.vidzee.wtf",
    "vide0.net",
    "voe.sx",
    "mixdrop.ag",
    "mixdrop.to",
    "streamtape.com",
    "dr0pstream.com",
    "embed.su",
    "2embed.cc",
    "multiembed.mov",
    "vidsrc.net",
];
static DIRECT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)https?://[^'"\s<>\\]*\.(?:m3u8|mp4)[^'"\s<>\\]*"#)
        .unwrap_or_else(|e| panic!("constant regex: {e}"))
});
static CONFIG: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)(?:['"]?(?:file|hls_?url|mp4_?url|stream_?url|file_?url|playback_?url)['"]?)\s*:\s*['"](https?://[^'"\s<>]+)['"]"#).unwrap_or_else(|e| panic!("constant regex: {e}"))
});
static BASE64: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"atob\(['"]([A-Za-z0-9+/=]{20,})['"]\)"#)
        .unwrap_or_else(|e| panic!("constant regex: {e}"))
});

/// The last embed-page fallback; dedicated extractors take priority.
#[derive(Debug, Default)]
pub struct EmbedResolver;
impl EmbedResolver {
    /// Create a stateless resolver.
    pub fn new() -> Self {
        Self
    }
}
#[async_trait]
impl Extractor for EmbedResolver {
    fn id(&self) -> &'static str {
        "embedresolver"
    }
    fn label(&self) -> &'static str {
        "Embed"
    }
    fn supports(&self, _: &ResolveCtx<'_>, url: &Url) -> bool {
        url.host_str().is_some_and(|host| {
            HOSTS
                .iter()
                .any(|suffix| host == *suffix || host.ends_with(&format!(".{suffix}")))
        })
    }
    async fn extract(
        &self,
        ctx: &ResolveCtx<'_>,
        url: &Url,
    ) -> Result<Vec<Stream>, ExtractorError> {
        let html = fetch_page(ctx, url).await?;
        let mut documents = vec![html.clone()];
        if let Some(unpacked) = vsources_core::unpack::unpack_eval(&html) {
            documents.push(unpacked);
        }
        for caps in BASE64.captures_iter(&html).flatten().take(16) {
            if let Some(encoded) = caps.get(1).filter(|m| m.as_str().len() <= 256 * 1024)
                && let Ok(bytes) = STANDARD.decode(encoded.as_str())
                && let Ok(decoded) = String::from_utf8(bytes)
            {
                documents.push(decoded);
            }
        }
        let mut seen = HashSet::new();
        let mut streams = Vec::new();
        for document in documents {
            let document = document
                .replace("\\/", "/")
                .replace("\\u0026", "&")
                .replace("&amp;", "&");
            let candidates = CONFIG
                .captures_iter(&document)
                .flatten()
                .filter_map(|c| c.get(1).map(|m| m.as_str().to_owned()))
                .chain(
                    DIRECT
                        .find_iter(&document)
                        .flatten()
                        .map(|m| m.as_str().to_owned()),
                );
            for raw in candidates {
                if let Ok(target) = Url::parse(&raw)
                    && !target.path().contains("favicon")
                    && seen.insert(target.clone())
                {
                    streams.push(direct_stream(
                        target.clone(),
                        format_for_url(&target),
                        Duration::from_mins(30),
                        url,
                    ));
                }
            }
        }
        if streams.is_empty() {
            Err(ExtractorError::NotFound)
        } else {
            Ok(streams)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{ScriptedFetcher, ctx_for};
    #[tokio::test]
    async fn resolves_deduplicated_player_urls_with_hotlink_headers()
    -> Result<(), Box<dyn std::error::Error>> {
        let encoded = STANDARD.encode(r#"{"file":"https://cdn.example/video.mp4"}"#);
        let fetcher = ScriptedFetcher::default().page("/embed", format!(r#"sources: [{{file: "https:\/\/cdn.example\/master.m3u8?a=1\u0026b=2"}}]; atob('{encoded}');"#));
        let embed = Url::parse("https://embed.su/embed")?;
        let referer = Url::parse("https://origin.example/watch")?;
        let ctx = ctx_for(&fetcher, Some(&referer));
        let streams = EmbedResolver::new().extract(&ctx, &embed).await?;
        assert_eq!(streams.len(), 2);
        assert_eq!(streams[0].url.query(), Some("a=1&b=2"));
        assert_eq!(
            fetcher.sent_header("/embed", "Referer").as_deref(),
            Some(referer.as_str())
        );
        assert_eq!(
            streams[0]
                .meta
                .request_headers
                .get("Referer")
                .map(String::as_str),
            Some("https://embed.su/")
        );
        Ok(())
    }
    #[test]
    fn matching_observes_hostname_boundaries() -> Result<(), url::ParseError> {
        let fetcher = ScriptedFetcher::default();
        let ctx = ctx_for(&fetcher, None);
        assert!(EmbedResolver::new().supports(&ctx, &Url::parse("https://www.vidsrc.me/embed")?));
        assert!(!EmbedResolver::new().supports(&ctx, &Url::parse("https://evilvidsrc.me/embed")?));
        Ok(())
    }
}
