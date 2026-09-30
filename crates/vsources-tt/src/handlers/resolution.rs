//! Resolution handlers (TS `handlers.ts` lines 120-190).

use crate::transforms::Transform;
use crate::types::Handler;

/// Resolution handlers in original order.
pub fn handlers() -> Vec<Handler> {
    vec![
        // Two resolution tokens: the later occurrence wins.
        Handler::new(
            "resolution",
            r"\b(?:4k|2160p|1080p|720p|480p)\b.+\b(4k|2160p|1080p|720p|480p)\b",
        )
        .with_transform(Transform::Lowercase)
        .with_remove()
        .with_match_group(1),
        Handler::new("resolution", r"\b[(\[]?4k[)\]]?\b")
            .with_transform(Transform::Value("4k".to_string()))
            .with_remove(),
        // `2160p` and its `21600p` typo both mean 4k.
        Handler::new("resolution", r"21600?[pi]")
            .with_transform(Transform::Value("4k".to_string()))
            .with_remove()
            .with_keep_matching(),
        Handler::new("resolution", r"[(\[]?3840x\d{4}[)\]]?")
            .with_transform(Transform::Value("4k".to_string()))
            .with_remove(),
        Handler::new("resolution", r"[(\[]?1920x\d{3,4}[)\]]?")
            .with_transform(Transform::Value("1080p".to_string()))
            .with_remove(),
        Handler::new("resolution", r"[(\[]?1280x\d{3}[)\]]?")
            .with_transform(Transform::Value("720p".to_string()))
            .with_remove(),
        // Custom aspect ratios: the height carries the resolution.
        Handler::new("resolution", r"[(\[]?\d{3,4}x(\d{3,4})[)\]]?")
            .with_transform(Transform::WithSuffix("p".to_string()))
            .with_remove(),
        // Typos such as `7200p` / `10800p`.
        Handler::new("resolution", r"(480|720|1080)0[pi]")
            .with_transform(Transform::WithSuffix("p".to_string()))
            .with_remove(),
        // Prefixed resolutions such as `BD1080` / `M1080`.
        Handler::new("resolution", r"(?:BD|HD|M)(720|1080|2160)")
            .with_transform(Transform::WithSuffix("p".to_string()))
            .with_remove(),
        Handler::new("resolution", r"(480|576|720|1080|2160)[pi]")
            .with_transform(Transform::WithSuffix("p".to_string()))
            .with_remove(),
        Handler::new("resolution", r"(?<!\d)(\d{3,4})[pi]")
            .with_transform(Transform::WithSuffix("p".to_string()))
            .with_remove(),
    ]
}

#[cfg(test)]
mod tests {
    use crate::parse_torrent_title;

    /// Cases taken from the TypeScript `resolution.test.ts`.
    #[test]
    fn parses_resolution() {
        let cases: &[(&str, &str)] = &[
            (
                "Annabelle.2014.1080p.PROPER.HC.WEBRip.x264.AAC.2.0-RARBG",
                "1080p",
            ),
            (
                "The Smurfs 2 2013 COMPLETE FULL BLURAY UHD (4K) - IPT EXCLUSIVE",
                "4k",
            ),
            ("Joker.2019.2160p.4K.BluRay.x265.10bit.HDR.AAC5.1", "4k"),
            (
                "IT Chapter Two.2019.7200p.AMZN WEB-DL.H264.[Eng Hin Tam Tel]DDP 5.1.MSubs.D0T.Telly",
                "720p",
            ),
            (
                "Life After People (2008) [1080P.BLURAY] [720p] [BluRay] [YTS.MX]",
                "720p",
            ),
        ];
        for (title, expected) in cases {
            let parsed = parse_torrent_title(title);
            assert_eq!(
                parsed.resolution.as_deref(),
                Some(*expected),
                "title: {title}"
            );
        }
    }
}
