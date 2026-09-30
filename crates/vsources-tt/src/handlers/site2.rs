//! Site handlers, final batch (TS `handlers.ts` lines 3128-3189).

use crate::transforms::Transform;
use crate::types::Handler;

/// Site handlers in original order.
pub fn handlers() -> Vec<Handler> {
    vec![
        Handler::new("site", r"\[eztv\]")
            .with_transform(Transform::Value("eztv.re".to_string()))
            .with_remove()
            .with_skip_from_title(),
        Handler::new("site", r"\beztv\b")
            .with_transform(Transform::Value("eztv.re".to_string()))
            .with_remove()
            .with_skip_from_title(),
        Handler::new("site", r"(\[([^\[\].]+\.[^\].]+)\])(?:\.\w{2,4}$|\s)")
            .with_transform(Transform::Trimmed)
            .with_remove()
            .with_match_group(1)
            .with_value_group(2),
        Handler::new("site", r"[\[{(](www.\w*.\w+)[)}\]]")
            .with_remove()
            .with_skip_from_title(),
        Handler::new(
            "site",
            r"[[(【].*?((?:www?.?)?(?:\w+-)?\w+(?:[.\s](?:com|org|net|ms|tv|mx|co|party|vip|nu|pics))\b).*?[\])】]",
        )
        .with_match_group(0)
        .with_remove()
        .with_skip_from_title(),
        Handler::new("site", r"-(www\.[\w-]+\.[\w-]+(?:\.[\w-]+)*)\.(\w{2,4})$")
            .with_transform(Transform::Trimmed)
            .with_remove()
            .with_skip_from_title()
            .with_match_group(1),
        Handler::new("site", r"\[([^\[\].]+\.[^\].]+)\](?:\.\w{2,4})?(?:$|\s)")
            .with_transform(Transform::Trimmed)
            .with_remove()
            .with_skip_from_title()
            .with_match_group(1),
        Handler::new("site", r"[\[{(](www\.[\w-]+\.[\w-]+(?:\.[\w-]+)*)[)}\]]")
            .with_transform(Transform::Trimmed)
            .with_remove()
            .with_skip_from_title()
            .with_match_group(1),
    ]
}

#[cfg(test)]
mod tests {
    use crate::parse_torrent_title;

    /// Table-driven cases ported from `site.test.ts`, asserting only the
    /// `site` field owned by this module.
    #[test]
    fn site_matches_ported_corpus() {
        let cases: &[(&str, Option<&str>)] = &[
            (
                "The Flash 2014 S01E01 (1080p AMZN Webrip x265 10bit EAC3 5 1 - Goki)[TAoE] [eztv]",
                Some("eztv.re"),
            ),
            (
                "The Flash 2014 S01E01 (1080p AMZN Webrip x265 10bit EAC3 5 1 - Goki)[TAoE] eztv",
                Some("eztv.re"),
            ),
            (
                "Anatomia De Grey - Temporada 19 [HDTV][Cap.1905][Castellano][www.AtomoHD.nu].avi",
                Some("www.AtomoHD.nu"),
            ),
            (
                "3 Musketeers 2011 R5 READNFO XviD-NYDIC [www.HD-ELITE.NET]",
                Some("www.HD-ELITE.NET"),
            ),
            (
                "Firefly.2002.04.Shindig.Vo+Stfr+Steng.1080p.WEB-DL.DD 2.0.H.264-P2P-www.Torrent9.cz.mkv",
                Some("www.Torrent9.cz"),
            ),
            (
                "[www.Naruto-Kun.Hu] Dragon Ball Z - 001.mkv",
                Some("www.Naruto-Kun.Hu"),
            ),
            (
                "[www.arabp2p.net]_-_تركي مترجم ومدبلج Last.Call.for.Istanbul.2023.1080p.NF.WEB-DL.DDP5.1.H.264.MKV.torrent",
                Some("www.arabp2p.net"),
            ),
            (
                "Breaking.Bad.S01.720p.BRRip.Hindi-English.ESUB - Cukister",
                None,
            ),
        ];
        for &(title, expected) in cases {
            let parsed = parse_torrent_title(title);
            assert_eq!(parsed.site.as_deref(), expected, "title: {title}");
        }
    }
}
