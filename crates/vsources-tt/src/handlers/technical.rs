//! Technical handlers: bit depth, HDR, 3D, codec, channels, audio, size,
//! and container (TS `handlers.ts` lines 854-1189, mirroring lines
//! 1056-1534 in the original Go `handlers.go`).

use crate::js_regex;
use crate::processors::Processor;
use crate::transforms::Transform;
use crate::types::Handler;
use crate::validators::Validator;

/// Technical handlers in original order.
// A verbatim data table; its length mirrors the TypeScript source section,
// and splitting it would obscure the 1:1 ordering that parsing relies on.
#[allow(clippy::too_many_lines)]
pub fn handlers() -> Vec<Handler> {
    vec![
        // Bit Depth handlers (TS lines 854-875).
        Handler::new("bitDepth", r"(?:8|10|12)[-.]?bit\b")
            .with_transform(Transform::Lowercase)
            .with_remove(),
        Handler::new("bitDepth", r"\bhevc\s?10\b")
            .with_transform(Transform::Value("10bit".to_string())),
        Handler::new("bitDepth", r"\bhdr10(?:\+|plus)?\b")
            .with_transform(Transform::Value("10bit".to_string())),
        Handler::new("bitDepth", r"\bhi10\b").with_transform(Transform::Value("10bit".to_string())),
        Handler::process_only("bitDepth", Processor::RemoveFromValue("[ -]".to_string())),
        // HDR handlers (TS lines 877-908).
        Handler::new("hdr", r"\bDV\b|dolby.?vision|\bDoVi\b")
            .with_transform(Transform::ValueSet("DV".to_string()))
            .with_remove()
            .with_keep_matching(),
        Handler::new("hdr", r"HDR10(?:\+|plus)")
            .with_transform(Transform::ValueSet("HDR10+".to_string()))
            .with_remove()
            .with_keep_matching(),
        Handler::new("hdr", r"\bHDR(?:10)?\b")
            .with_transform(Transform::ValueSet("HDR".to_string()))
            .with_remove()
            .with_keep_matching(),
        Handler::new("hdr", r"\bSDR\b")
            .with_transform(Transform::ValueSet("SDR".to_string()))
            .with_remove()
            .with_keep_matching(),
        // 3D handlers (TS lines 910-956).
        Handler::new("threeD", r"\b(3D)\b.*\b(Half-?SBS|H[-\\/]?SBS)\b")
            .with_transform(Transform::Value("3D HSBS".to_string())),
        Handler::new("threeD", r"\bHalf.Side.?By.?Side\b")
            .with_transform(Transform::Value("3D HSBS".to_string())),
        Handler::new("threeD", r"\b(3D)\b.*\b(Full-?SBS|SBS)\b")
            .with_transform(Transform::Value("3D SBS".to_string())),
        Handler::new("threeD", r"\bSide.?By.?Side\b")
            .with_transform(Transform::Value("3D SBS".to_string())),
        Handler::new("threeD", r"\b(3D)\b.*\b(Half-?OU|H[-\\/]?OU)\b")
            .with_transform(Transform::Value("3D HOU".to_string())),
        Handler::new("threeD", r"\bHalf.?Over.?Under\b")
            .with_transform(Transform::Value("3D HOU".to_string())),
        Handler::new("threeD", r"\b(3D)\b.*\b(OU)\b")
            .with_transform(Transform::Value("3D OU".to_string())),
        Handler::new("threeD", r"\bOver.?Under\b")
            .with_transform(Transform::Value("3D OU".to_string())),
        Handler::new("threeD", r"\b((?:BD)?3D)\b")
            .with_transform(Transform::Value("3D".to_string()))
            .with_skip_if_first(),
        // Codec handlers (TS lines 958-996).
        Handler::new("codec", r"\b[xh][-. ]?26[45]")
            .with_transform(Transform::Lowercase)
            .with_remove(),
        Handler::new("codec", r"\bhevc(?:\s?10)?\b")
            .with_transform(Transform::Value("hevc".to_string()))
            .with_remove()
            .with_keep_matching(),
        Handler::new("codec", r"\b(?:dvix|mpeg2|divx|xvid|avc)\b")
            .with_transform(Transform::Lowercase)
            .with_remove()
            .with_keep_matching(),
        Handler::new("codec", r"\bvp[89]\b")
            .with_transform(Transform::Lowercase)
            .with_remove()
            .with_keep_matching(),
        Handler::new("codec", r"\bAV1\b")
            .with_transform(Transform::Value("av1".to_string()))
            .with_remove()
            .with_keep_matching(),
        Handler::process_only("codec", Processor::RemoveFromValue("[ .-]".to_string())),
        // Channels handlers (TS lines 998-1058).
        Handler::new("channels", r"\bDDP?(51)\b")
            .with_transform(Transform::ValueSet("5.1".to_string()))
            .with_keep_matching()
            .with_remove()
            .with_match_group(1),
        Handler::new("channels", r"5[.\s]1(?:ch|-S\d+)?\b")
            .with_transform(Transform::ValueSet("5.1".to_string()))
            .with_keep_matching()
            .with_remove(),
        Handler::new("channels", r"\b(?:x[2-4]|5[\W]1(?:x[2-4])?)\b")
            .with_transform(Transform::ValueSet("5.1".to_string()))
            .with_keep_matching()
            .with_remove(),
        Handler::new("channels", r"\b7[.\- ]1(?:.?ch(?:annel)?)?\b")
            .with_transform(Transform::ValueSet("7.1".to_string()))
            .with_keep_matching()
            .with_remove(),
        Handler::new("channels", r"(?:\b|AAC|DDP)\+?(2[.\s]0)(?:x[2-4])?\b")
            .with_validator(Validator::lookahead(r"[.\s](?:19|20)\d{2}\b", true, false))
            .with_skip_if_before(&["year"])
            .with_transform(Transform::ValueSet("2.0".to_string()))
            .with_keep_matching()
            .with_skip_from_title()
            .with_match_group(1),
        Handler::new("channels", r"\b2\.0\b")
            .with_validator(Validator::lookahead(r"[.\s](?:19|20)\d{2}\b", true, false))
            .with_skip_if_before(&["year"])
            .with_transform(Transform::ValueSet("2.0".to_string()))
            .with_keep_matching()
            .with_skip_from_title(),
        Handler::new("channels", r"\bstereo\b")
            .with_transform(Transform::ValueSet("stereo".to_string()))
            .with_keep_matching(),
        Handler::new("channels", r"\bmono\b")
            .with_transform(Transform::ValueSet("mono".to_string()))
            .with_keep_matching(),
        // Audio handlers (TS lines 1060-1100).
        Handler::new("audio", r"\b(?:.+HR)?(?:DTS.?HD.?Ma(?:ster)?|DTS.?X)\b")
            .with_validator(Validator::NotMatch(
                js_regex::compile_ci(r"(?:.+HR)").unwrap_or_else(js_regex::never_match),
            ))
            .with_transform(Transform::ValueSet("DTS Lossless".to_string()))
            .with_remove()
            .with_keep_matching(),
        Handler::new(
            "audio",
            r"\bDTS(?:(?:.?HD.?Ma(?:ster)?|.X))?.?(?:HD.?HR|HD)?\b",
        )
        .with_validator(Validator::NotMatch(
            js_regex::compile_ci(r"DTS(?:.?HD.?Ma(?:ster)?|.X)")
                .unwrap_or_else(js_regex::never_match),
        ))
        .with_transform(Transform::ValueSet("DTS Lossy".to_string()))
        .with_remove()
        .with_keep_matching(),
        Handler::new("audio", r"\b(?:Dolby.?)?Atmos\b")
            .with_transform(Transform::ValueSet("Atmos".to_string()))
            .with_remove()
            .with_keep_matching(),
        Handler::new("audio", r"\bTrue[ .-]?HD\b")
            .with_transform(Transform::ValueSet("TrueHD".to_string()))
            .with_keep_matching()
            .with_remove()
            .with_skip_from_title(),
        Handler::new_case_sensitive("audio", r"\bTRUE\b")
            .with_transform(Transform::ValueSet("TrueHD".to_string()))
            .with_keep_matching()
            .with_remove()
            .with_skip_from_title()
            .with_skip_if_before(&["year", "seasons", "episodes"]),
        // More Audio handlers (TS lines 1102-1166).
        Handler::new("audio", r"\bFLAC(?:\d\.\d)?(?:x\d+)?\b")
            .with_transform(Transform::ValueSet("FLAC".to_string()))
            .with_keep_matching()
            .with_remove(),
        Handler::new("audio", r"\bDD2?[+p]|DD Plus|Dolby Digital Plus|DDP5[ ._]1")
            .with_transform(Transform::ValueSet("DDP".to_string()))
            .with_keep_matching()
            .with_remove(),
        Handler::new("audio", r"E-?AC-?3(?:-S\d+)?")
            .with_transform(Transform::ValueSet("EAC3".to_string()))
            .with_keep_matching()
            .with_remove(),
        Handler::new("audio", r"\b(DD|Dolby.?Digital|DolbyD)\b")
            .with_transform(Transform::ValueSet("DD".to_string()))
            .with_keep_matching()
            .with_remove(),
        Handler::new("audio", r"\b(AC-?3D?(?:x2)?(?:-S\d+)?)\b")
            .with_transform(Transform::ValueSet("AC3".to_string()))
            .with_keep_matching()
            .with_remove(),
        Handler::new_case_sensitive("audio", r"\bQ?AAC(?:[. ]?2[. ]0|x2)?\b")
            .with_transform(Transform::ValueSet("AAC".to_string()))
            .with_keep_matching()
            .with_remove(),
        Handler::new("audio", r"\bL?PCM\b")
            .with_transform(Transform::ValueSet("PCM".to_string()))
            .with_keep_matching()
            .with_remove(),
        Handler::new("audio", r"\bOPUS(?:\b|\d)(?:.*[ ._-](?:\d{3,4}p))?")
            .with_validator(Validator::NotMatch(
                js_regex::compile_ci(r"OPUS(?:\b|\d)(?:.*[ ._-](?:\d{3,4}p))")
                    .unwrap_or_else(js_regex::never_match),
            ))
            .with_transform(Transform::ValueSet("OPUS".to_string()))
            .with_keep_matching()
            .with_remove(),
        Handler::new("audio", r"\b(?:H[DQ])?.?(?:Clean.?Aud(?:io)?)\b")
            .with_transform(Transform::ValueSet("HQ".to_string()))
            .with_remove()
            .with_keep_matching(),
        Handler::new_case_sensitive("channels", r"\[([257][.-][01])]")
            .with_transform(Transform::ValueSetTransform(false))
            .with_remove()
            .with_keep_matching(),
        // Size handler (TS lines 1174-1181).
        Handler::new("size", r"\b(\d+((\.|,)\d+)?[\s-]?(MB|GB|TB))\b").with_remove(),
        // Container handler (TS lines 1183-1188).
        Handler::new(
            "container",
            r"\.?[\[(]?\b(MKV|AVI|MP4|WMV|MPG|MPEG)\b[\])]?",
        )
        .with_transform(Transform::Lowercase),
    ]
}

#[cfg(test)]
mod tests {
    use crate::parse_torrent_title;

    /// Cases ported from `codec.test.ts` and `audio.test.ts`, asserting only
    /// the fields owned by this module.
    #[test]
    fn codec_and_bit_depth_match_ported_corpus() {
        // `HEVC10bit` only splits after the `10bit` handler removes its
        // match, so the bit-depth-before-codec order is load-bearing.
        let parsed = parse_torrent_title(
            "[Anime Time] Re Zero kara Hajimeru Isekai Seikatsu \
             (Season 2 Part 1) [1080p][HEVC10bit x265][Multi Sub]",
        );
        assert_eq!(parsed.bit_depth.as_deref(), Some("10bit"));
        assert_eq!(parsed.codec.as_deref(), Some("hevc"));

        let parsed = parse_torrent_title("Nocturnal Animals 2016 VFF 1080p BluRay DTS HEVC-HD2");
        assert_eq!(parsed.codec.as_deref(), Some("hevc"));

        let parsed = parse_torrent_title("doctor_who_2005.8x12.death_in_heaven.720p_hdtv_x264-fov");
        assert_eq!(parsed.codec.as_deref(), Some("x264"));
    }

    /// Cases ported from `audio.test.ts`; `audio`, `channels`, and `size`
    /// are value-set or plain fields owned by this module.
    #[test]
    fn audio_and_channels_match_ported_corpus() {
        let parsed = parse_torrent_title("Gold 2016 1080p BluRay DTS-HD MA 5 1 x264-HDH");
        assert_eq!(parsed.audio, Some(vec!["DTS Lossless".to_string()]));

        let parsed = parse_torrent_title("Rain Man 1988 REMASTERED 1080p BRRip x264 AAC-m2g");
        assert_eq!(parsed.audio, Some(vec!["AAC".to_string()]));

        let parsed =
            parse_torrent_title("Condor.S01E03.1080p.WEB-DL.x265.10bit.EAC3.6.0-Qman[UTR].mkv");
        assert_eq!(parsed.audio, Some(vec!["EAC3".to_string()]));
        assert_eq!(parsed.container.as_deref(), Some("mkv"));
    }

    /// The canonical example title from the crate docs.
    #[test]
    fn matrix_title_reports_technical_fields() {
        let parsed = parse_torrent_title("The.Matrix.1999.1080p.BluRay.x264.DTS-HD.MA.5.1");
        assert_eq!(parsed.codec.as_deref(), Some("x264"));
        assert_eq!(parsed.audio, Some(vec!["DTS Lossless".to_string()]));
        assert_eq!(parsed.channels, Some(vec!["5.1".to_string()]));
    }
}
