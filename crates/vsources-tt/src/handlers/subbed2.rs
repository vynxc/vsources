//! Subbed detection handlers (TS `handlers.ts` lines 3038-3075).

use crate::processors::Processor;
use crate::transforms::Transform;
use crate::types::Handler;

/// Subbed handlers in original order.
pub fn handlers() -> Vec<Handler> {
    vec![
        Handler::new(
            "subbed",
            r"\b(?:DAN|E|FIN|PL|SLO|SWE|HEB|NOR|GER|ITA|SPA|POR|DUT|NL|CZE|GRE|TUR|ARA|RUS|HUN|HIN|ENG|KOR|JPN|CHI|VIE|THA)SUBS?\b",
        )
        .with_transform(Transform::Boolean)
        .with_keep_matching()
        .with_remove()
        .with_skip_if_first(),
        Handler::new("subbed", r"\b(?:Official.*?|Dual-?)?sub(?:s|bed)?\b")
            .with_transform(Transform::Boolean)
            .with_remove()
            .with_skip_if_first(),
        Handler::new("subbed", r"\b(?:Official.*?|Dual-?)sub(?:s|bed)?\b")
            .with_transform(Transform::Boolean)
            .with_remove(),
        Handler::process_only("subbed", Processor::SubbedFromLanguages),
    ]
}

#[cfg(test)]
mod tests {
    use crate::parse_torrent_title;

    /// Cases from the TypeScript corpus (`languages.test.ts` and
    /// `episodeTitle.test.ts`); only the `subbed` field (this module's
    /// field) is asserted.
    #[test]
    fn subbed_table_cases() {
        let cases: &[(&str, Option<bool>)] = &[
            // `LANGSUB` prefixes (`episodeTitle.test.ts`).
            ("One.Piece.S01E01.HebSub.XviD", Some(true)),
            // KORSUB (`languages.test.ts`).
            ("The.Nun.2018.KORSUB.HDRip.XviD.MP3-STUTTERSHIT", Some(true)),
            // Bare `sub(s|bed)` tags (`languages.test.ts`).
            (
                "1917 2019 1080p Bluray x264-Sexmeup [Greek Subs] [Braveheart]",
                Some(true),
            ),
            ("Dilbert complete series + en subs", Some(true)),
            (
                "House S 1 CD 1-6 svensk, danska, norsk, finsk sub",
                Some(true),
            ),
            (
                "[Hakata Ramen] Hoshiai No Sora (Stars Align) 01 [1080p][HEVC][x265][10bit][Dual-Subs] HR-DR",
                Some(true),
            ),
            // `multi subs` in the languages set feeds the process handler.
            (
                "Casablanca 1942 BDRip 1080p [multi language,multi subs].mkv",
                Some(true),
            ),
            (
                "Avengers.Endgame.2019.4K.UHD.ITUNES.DL.H265.Dolby.ATMOS.MSUBS-Deflate.Telly",
                Some(true),
            ),
            (
                "FernGully [H264 - Ita Dut Fre Ger Eng Spa Aac - MultiSub]",
                Some(true),
            ),
            (
                "Mommie Dearest [1981 PAL DVD][En.De.Fr.It.Es Multisubs[18]",
                Some(true),
            ),
            (
                "Patriot Games [1992] Eng, Ger, Cze, Hun, Pol + multisub  DVDrip",
                Some(true),
            ),
            // No `sub` tag at a word boundary.
            ("Atonement.2017.KOREAN.ENSUBBED.1080p.WEBRip.x264-VXT", None),
            ("Thai Massage (2022) 720p PDVDRip x264 AAC.mkv", None),
            ("Get Him to the Greek 2010 720p BluRay", None),
        ];
        for (title, expected) in cases {
            let parsed = parse_torrent_title(title);
            assert_eq!(parsed.subbed, *expected, "title: {title}");
        }
    }

    /// The process handler only fires when the languages set (owned by the
    /// earlier language sections) contains `multi subs`.
    #[test]
    fn subbed_process_handler_reads_languages() {
        let casablanca =
            parse_torrent_title("Casablanca 1942 BDRip 1080p [multi language,multi subs].mkv");
        assert_eq!(
            casablanca.languages,
            Some(vec!["multi subs".to_string(), "multi audio".to_string()])
        );
        assert_eq!(casablanca.subbed, Some(true));

        let endgame = parse_torrent_title(
            "Avengers.Endgame.2019.4K.UHD.ITUNES.DL.H265.Dolby.ATMOS.MSUBS-Deflate.Telly",
        );
        assert_eq!(endgame.languages, Some(vec!["multi subs".to_string()]));
        assert_eq!(endgame.subbed, Some(true));
    }
}
