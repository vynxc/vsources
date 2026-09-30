//! Detailed language detection handlers: every individual language from
//! English through Persian, plus the Unicode script-range fallbacks
//! (TS `handlers.ts` lines 2075-3007).

use crate::transforms::Transform;
use crate::types::Handler;
use crate::validators::Validator;

/// Detailed language handlers in original order.
// A verbatim data table; its length mirrors the TypeScript source section,
// and splitting it would obscure the 1:1 ordering that parsing relies on.
#[allow(clippy::too_many_lines)]
pub fn handlers() -> Vec<Handler> {
    vec![
        // English language handlers (TS lines 2075-2103).
        Handler::new("languages", r"\bengl?(?:sub[A-Z]*)?\b")
            .with_transform(Transform::ValueSet("en".to_string()))
            .with_keep_matching(),
        Handler::new("languages", r"\beng?sub[A-Z]*\b")
            .with_transform(Transform::ValueSet("en".to_string()))
            .with_keep_matching(),
        Handler::new("languages", r"\bing(?:l[eéê]s)?\b")
            .with_transform(Transform::ValueSet("en".to_string()))
            .with_keep_matching(),
        Handler::new("languages", r"\besub\b")
            .with_transform(Transform::ValueSet("en".to_string()))
            .with_keep_matching()
            .with_remove(),
        Handler::new("languages", r"\benglish\W+(?:subs?|sdh|hi)\b")
            .with_transform(Transform::ValueSet("en".to_string()))
            .with_keep_matching(),
        Handler::new("languages", r"\bEN\b")
            .with_transform(Transform::ValueSet("en".to_string()))
            .with_keep_matching()
            .with_skip_from_title(),
        Handler::new("languages", r"\benglish?\b")
            .with_transform(Transform::ValueSet("en".to_string()))
            .with_keep_matching()
            .with_skip_if_first(),
        // Japanese language handlers (TS lines 2122-2135).
        Handler::new("languages", r"\b(?:JP|JAP|JPN)\b")
            .with_transform(Transform::ValueSet("ja".to_string()))
            .with_keep_matching(),
        Handler::new("languages", r"(japanese|japon[eê]s)\b")
            .with_transform(Transform::ValueSet("ja".to_string()))
            .with_keep_matching()
            .with_skip_if_first(),
        // Korean language handlers (TS lines 2137-2149).
        Handler::new("languages", r"\b(?:KOR|kor[ .-]?sub)\b")
            .with_transform(Transform::ValueSet("ko".to_string()))
            .with_keep_matching(),
        Handler::new("languages", r"(korean|coreano)\b")
            .with_transform(Transform::ValueSet("ko".to_string()))
            .with_keep_matching()
            .with_skip_if_first(),
        // Chinese language handlers (TS lines 2152-2191).
        Handler::new(
            "languages",
            r"\b(?:traditional\W*chinese|chinese\W*traditional)(?:\Wchi)?\b",
        )
        .with_transform(Transform::ValueSet("zh-tw".to_string()))
        .with_keep_matching()
        .with_remove(),
        Handler::new("languages", r"\bzh-hant\b")
            .with_transform(Transform::ValueSet("zh-tw".to_string()))
            .with_keep_matching(),
        Handler::new("languages", r"\b(?:mand[ae]rin|ch[sn])\b")
            .with_transform(Transform::ValueSet("zh".to_string()))
            .with_keep_matching(),
        Handler::new("languages", r"(?:shang-?)?\bCH(?:I|T)\b")
            .with_validator(Validator::NotMatch(
                crate::js_regex::compile_ci(r"shang-?")
                    .unwrap_or_else(crate::js_regex::never_match),
            ))
            .with_transform(Transform::ValueSet("zh".to_string()))
            .with_keep_matching()
            .with_skip_from_title(),
        Handler::new("languages", r"\b(chinese|chin[eê]s)\b")
            .with_transform(Transform::ValueSet("zh".to_string()))
            .with_keep_matching()
            .with_skip_if_first(),
        Handler::new("languages", r"\bzh-hans\b")
            .with_transform(Transform::ValueSet("zh".to_string()))
            .with_keep_matching(),
        // French language handlers (TS lines 2194-2221).
        Handler::new("languages", r"\bFR(?:a|e|anc[eê]s|VF[FQIB2]?)\b")
            .with_transform(Transform::ValueSet("fr".to_string()))
            .with_keep_matching()
            .with_skip_from_title(),
        Handler::new_case_sensitive("languages", r"\b(?:TRUE|SUB).?FRENCH\b|\bFRENCH\b|\bFre?\b")
            .with_transform(Transform::ValueSet("fr".to_string()))
            .with_keep_matching()
            .with_remove(),
        Handler::new_case_sensitive("languages", r"\b\[?(?:VF[FQRIB2]?\]?\b|(?:VOST)?FR2?)\b")
            .with_transform(Transform::ValueSet("fr".to_string()))
            .with_keep_matching()
            .with_remove(),
        Handler::new("languages", r"\bVOST(?:FR?|A)?\b")
            .with_transform(Transform::ValueSet("fr".to_string()))
            .with_keep_matching(),
        // Spanish/Latino language handlers (TS lines 2223-2285).
        Handler::new(
            "languages",
            r"\b(?:spanish\W?latin|american\W*(?:spa|esp?))\b",
        )
        .with_transform(Transform::ValueSet("es-419".to_string()))
        .with_keep_matching()
        .with_remove()
        .with_skip_from_title(),
        Handler::new("languages", r"\b(?:audio.)?lat(?:in?|ino)?\b")
            .with_transform(Transform::ValueSet("es-419".to_string()))
            .with_keep_matching(),
        Handler::new(
            "languages",
            r"\b(?:audio.)?(?:ESP?|spa|(?:en[ .]+)?espa[nñ]ola?|castellano)\b",
        )
        .with_transform(Transform::ValueSet("es".to_string()))
        .with_keep_matching(),
        Handler::new("languages", r"\bes(?:\.(?:ass|ssa|srt|sub|idx)$)")
            .with_transform(Transform::ValueSet("es".to_string()))
            .with_keep_matching()
            .with_skip_from_title(),
        Handler::new("languages", r"\bspanish\W+subs?\b")
            .with_transform(Transform::ValueSet("es".to_string()))
            .with_keep_matching(),
        Handler::new("languages", r"\b(spanish|espanhol)\b")
            .with_transform(Transform::ValueSet("es".to_string()))
            .with_keep_matching()
            .with_skip_if_first(),
        Handler::new("languages", r"\bSP\b")
            .with_validator(Validator::And(vec![
                Validator::lookbehind(r"(?:w{3}\.\w+\.)", true, false),
                Validator::Or(vec![
                    Validator::lookahead(r"(?:[ .,/-]+(?:[A-Z]{2}[ .,/-]+){2,})", true, true),
                    Validator::lookbehind(r"(?:(?:[ .,/\[-]+[A-Z]{2}){2,}[ .,/-]+)", true, true),
                    Validator::And(vec![
                        Validator::lookahead(r"(?:[ .,/-]+[A-Z]{2}(?:[ .,/-]|$))", true, true),
                        Validator::lookbehind(r"(?:[ .,/\[-]+[A-Z]{2}[ .,/-]+)", true, true),
                    ]),
                ]),
            ]))
            .with_transform(Transform::ValueSet("es".to_string()))
            .with_keep_matching()
            .with_remove(),
        // Portuguese language handlers (TS lines 2287-2347).
        Handler::new("languages", r"\b(?:p[rt]|en|port)[. (\\/-]*BR\b")
            .with_transform(Transform::ValueSet("pt".to_string()))
            .with_keep_matching()
            .with_remove(),
        Handler::new("languages", r"\bbr(?:a|azil|azilian)\W+(?:pt|por)\b")
            .with_transform(Transform::ValueSet("pt".to_string()))
            .with_keep_matching()
            .with_remove(),
        Handler::new(
            "languages",
            r"\b(?:leg(?:endado|endas?)?|dub(?:lado)?|portugu[eèê]se?)[. -]*BR\b",
        )
        .with_transform(Transform::ValueSet("pt".to_string()))
        .with_keep_matching(),
        Handler::new("languages", r"\bleg(?:endado|endas?)\b")
            .with_transform(Transform::ValueSet("pt".to_string()))
            .with_keep_matching(),
        Handler::new("languages", r"\bportugu[eèê]s[ea]?\b")
            .with_transform(Transform::ValueSet("pt".to_string()))
            .with_keep_matching(),
        Handler::new("languages", r"\bPT[. -]*(?:PT|ENG?|sub(?:s|titles?))\b")
            .with_transform(Transform::ValueSet("pt".to_string()))
            .with_keep_matching(),
        Handler::new("languages", r"\bpt(?:\.(?:ass|ssa|srt|sub|idx)$)")
            .with_transform(Transform::ValueSet("pt".to_string()))
            .with_keep_matching()
            .with_skip_from_title(),
        Handler::new("languages", r"\bpt\b")
            .with_transform(Transform::ValueSet("pt".to_string()))
            .with_keep_matching()
            .with_remove(),
        Handler::new("languages", r"\bpor\b")
            .with_transform(Transform::ValueSet("pt".to_string()))
            .with_keep_matching()
            .with_skip_from_title(),
        // Italian language handlers (TS lines 2349-2388).
        Handler::new("languages", r"\bITA\b")
            .with_transform(Transform::ValueSet("it".to_string()))
            .with_keep_matching(),
        Handler::new("languages", r"\bIT\b")
            .with_validator(Validator::And(vec![
                Validator::lookbehind(r"(?:w{3}\.\w+\.)", true, false),
                Validator::Or(vec![
                    Validator::lookahead(r"(?:[ .,/-]+(?:[A-Z]{2}[ .,/-]+){2,})", true, true),
                    Validator::lookbehind(r"(?:(?:[ .,/\[-]+[A-Z]{2}){2,}[ .,/-]+)", true, true),
                ]),
            ]))
            .with_transform(Transform::ValueSet("it".to_string()))
            .with_keep_matching()
            .with_skip_from_title(),
        Handler::new("languages", r"\bit")
            .with_validator(Validator::lookahead(
                r"(?:\.(?:ass|ssa|srt|sub|idx)$)",
                true,
                true,
            ))
            .with_transform(Transform::ValueSet("it".to_string()))
            .with_keep_matching()
            .with_skip_from_title(),
        Handler::new("languages", r"\bitaliano?\b")
            .with_transform(Transform::ValueSet("it".to_string()))
            .with_keep_matching()
            .with_skip_if_first(),
        // Greek language handlers (TS lines 2390-2397).
        Handler::new(
            "languages",
            r"\bgreek[ .-]*(?:audio|lang(?:uage)?|subs?(?:titles?)?)?\b",
        )
        .with_transform(Transform::ValueSet("el".to_string()))
        .with_keep_matching()
        .with_skip_if_first(),
        // German language handlers (TS lines 2399-2455).
        Handler::new("languages", r"\b(?:GER|DEU)\b")
            .with_transform(Transform::ValueSet("de".to_string()))
            .with_keep_matching()
            .with_skip_from_title(),
        Handler::new("languages", r"\bde\b")
            .with_validator(Validator::lookahead(
                r"(?:[ .,/-]+(?:[A-Z]{2}[ .,/-]+){2,})",
                true,
                true,
            ))
            .with_transform(Transform::ValueSet("de".to_string()))
            .with_keep_matching()
            .with_skip_from_title(),
        Handler::new("languages", r"\bde\b")
            .with_validator(Validator::lookbehind(
                r"(?:[ .,/-]+(?:[A-Z]{2}[ .,/-]+){2,})",
                true,
                true,
            ))
            .with_transform(Transform::ValueSet("de".to_string()))
            .with_keep_matching()
            .with_skip_from_title(),
        Handler::new("languages", r"\bde\b")
            .with_validator(Validator::And(vec![
                Validator::lookbehind(r"(?:[ .,/-]+[A-Z]{2}[ .,/-]+)", true, true),
                Validator::lookahead(r"(?:[ .,/-]+[A-Z]{2}[ .,/-]+)", true, true),
            ]))
            .with_transform(Transform::ValueSet("de".to_string()))
            .with_keep_matching()
            .with_skip_from_title(),
        Handler::new("languages", r"\bde(?:\.(?:ass|ssa|srt|sub|idx)$)")
            .with_transform(Transform::ValueSet("de".to_string()))
            .with_keep_matching()
            .with_skip_from_title(),
        Handler::new("languages", r"\b(german|alem[aã]o)\b")
            .with_transform(Transform::ValueSet("de".to_string()))
            .with_keep_matching()
            .with_skip_if_first(),
        // Russian language handlers (TS lines 2457-2468).
        Handler::new("languages", r"\bRUS?\b")
            .with_transform(Transform::ValueSet("ru".to_string()))
            .with_keep_matching(),
        Handler::new("languages", r"(russian|russo)\b")
            .with_transform(Transform::ValueSet("ru".to_string()))
            .with_keep_matching()
            .with_skip_if_first(),
        // Ukrainian language handlers (TS lines 2472-2483).
        Handler::new("languages", r"\bUKR\b")
            .with_transform(Transform::ValueSet("uk".to_string()))
            .with_keep_matching(),
        Handler::new("languages", r"\bukrainian\b")
            .with_transform(Transform::ValueSet("uk".to_string()))
            .with_keep_matching()
            .with_skip_if_first(),
        // Indian language handlers (TS lines 2487-2557).
        Handler::new("languages", r"\bhin(?:di)?\b")
            .with_transform(Transform::ValueSet("hi".to_string()))
            .with_keep_matching(),
        Handler::new(
            "languages",
            r"\b(?:(?:w{3}\.\w+\.)?tel(?:\W*aviv)?|telugu)\b",
        )
        .with_validator(Validator::NotMatch(
            crate::js_regex::compile_ci(r"(?:(?:w{3}\.\w+\.)tel)|(?:tel(?:\W*aviv))")
                .unwrap_or_else(crate::js_regex::never_match),
        ))
        .with_transform(Transform::ValueSet("te".to_string()))
        .with_keep_matching(),
        Handler::new("languages", r"\bt[aâ]m(?:il)?\b")
            .with_transform(Transform::ValueSet("ta".to_string()))
            .with_keep_matching(),
        Handler::new("languages", r"\b(?:(?:w{3}\.\w+\.)?MAL(?:ay)?|malayalam)\b")
            .with_validator(Validator::NotMatch(
                crate::js_regex::compile_ci(r"\b(?:(?:w{3}\.\w+\.)MAL)\b")
                    .unwrap_or_else(crate::js_regex::never_match),
            ))
            .with_transform(Transform::ValueSet("ml".to_string()))
            .with_keep_matching()
            .with_remove()
            .with_skip_if_first(),
        Handler::new("languages", r"\b(?:(?:w{3}\.\w+\.)?KAN(?:nada)?|kannada)\b")
            .with_validator(Validator::NotMatch(
                crate::js_regex::compile_ci(r"\b(?:(?:w{3}\.\w+\.)KAN)\b")
                    .unwrap_or_else(crate::js_regex::never_match),
            ))
            .with_transform(Transform::ValueSet("kn".to_string()))
            .with_keep_matching()
            .with_remove(),
        Handler::new(
            "languages",
            r"\b(?:(?:w{3}\.\w+\.)?MAR(?:a(?:thi)?)?|marathi)\b",
        )
        .with_validator(Validator::NotMatch(
            crate::js_regex::compile_ci(r"\b(?:(?:w{3}\.\w+\.)MAR)\b")
                .unwrap_or_else(crate::js_regex::never_match),
        ))
        .with_transform(Transform::ValueSet("mr".to_string()))
        .with_keep_matching(),
        Handler::new(
            "languages",
            r"\b(?:(?:w{3}\.\w+\.)?GUJ(?:arati)?|gujarati)\b",
        )
        .with_validator(Validator::NotMatch(
            crate::js_regex::compile_ci(r"\b(?:(?:w{3}\.\w+\.)GUJ)\b")
                .unwrap_or_else(crate::js_regex::never_match),
        ))
        .with_transform(Transform::ValueSet("gu".to_string()))
        .with_keep_matching(),
        Handler::new("languages", r"\b(?:(?:w{3}\.\w+\.)?PUN(?:jabi)?|punjabi)\b")
            .with_validator(Validator::NotMatch(
                crate::js_regex::compile_ci(r"\b(?:(?:w{3}\.\w+\.)PUN)\b")
                    .unwrap_or_else(crate::js_regex::never_match),
            ))
            .with_transform(Transform::ValueSet("pa".to_string()))
            .with_keep_matching(),
        Handler::new(
            "languages",
            r"\b(?:(?:w{3}\.\w+\.)?BEN(?:.\bThe|and|of\b)?(?:gali)?|bengali)\b",
        )
        .with_validator(Validator::NotMatch(
            crate::js_regex::compile_ci(r"\b(?:(?:w{3}\.\w+\.)BEN)|(?:BEN)(?:.\bThe|and|of\b)\b")
                .unwrap_or_else(crate::js_regex::never_match),
        ))
        .with_transform(Transform::ValueSet("bn".to_string()))
        .with_keep_matching()
        .with_skip_if_first(),
        // Baltic language handlers (TS lines 2559-2585).
        Handler::new("languages", r"\b(?:YTS\.)?LT\b")
            .with_validator(Validator::NotMatch(
                crate::js_regex::compile_ci(r"(?:YTS\.)")
                    .unwrap_or_else(crate::js_regex::never_match),
            ))
            .with_transform(Transform::ValueSet("lt".to_string()))
            .with_keep_matching()
            .with_skip_from_title(),
        Handler::new("languages", r"\blithuanian\b")
            .with_transform(Transform::ValueSet("lt".to_string()))
            .with_keep_matching()
            .with_skip_if_first(),
        Handler::new("languages", r"\blatvian\b")
            .with_transform(Transform::ValueSet("lv".to_string()))
            .with_keep_matching()
            .with_skip_if_first(),
        Handler::new("languages", r"\bestonian\b")
            .with_transform(Transform::ValueSet("et".to_string()))
            .with_keep_matching()
            .with_skip_if_first(),
        // Polish language handlers (TS lines 2590-2615).
        Handler::new(
            "languages",
            r"\b(?:PLDUB|Dub(?:bing.?)?PL|Lek(?:tor.?)?PL|Film.Polski)\b",
        )
        .with_transform(Transform::ValueSet("pl".to_string()))
        .with_keep_matching()
        .with_remove(),
        Handler::new("languages", r"\b(?:Napisy.PL|PLSUB(?:BED)?)\b")
            .with_transform(Transform::ValueSet("pl".to_string()))
            .with_keep_matching()
            .with_remove(),
        Handler::new("languages", r"\b(?:(?:w{3}\.\w+\.)?PL|pol)\b")
            .with_validator(Validator::NotMatch(
                crate::js_regex::compile_ci(r"(?:w{3}\.\w+\.)")
                    .unwrap_or_else(crate::js_regex::never_match),
            ))
            .with_transform(Transform::ValueSet("pl".to_string()))
            .with_keep_matching(),
        Handler::new("languages", r"\b(polish|polon[eê]s|polaco)\b")
            .with_transform(Transform::ValueSet("pl".to_string()))
            .with_keep_matching()
            .with_skip_if_first(),
        // Czech/Slovak language handlers (TS lines 2620-2641).
        Handler::new("languages", r"\bCZ[EH]?\b")
            .with_transform(Transform::ValueSet("cs".to_string()))
            .with_keep_matching()
            .with_skip_if_first(),
        Handler::new("languages", r"\bczech\b")
            .with_transform(Transform::ValueSet("cs".to_string()))
            .with_keep_matching()
            .with_skip_if_first(),
        Handler::new("languages", r"\bslo(?:vak|vakian|subs|[\]_)]?\.\w{2,4}$)\b")
            .with_transform(Transform::ValueSet("sk".to_string()))
            .with_keep_matching()
            .with_skip_from_title(),
        // Hungarian language handlers (TS lines 2643-2656).
        Handler::new("languages", r"\bHU\b")
            .with_transform(Transform::ValueSet("hu".to_string()))
            .with_keep_matching()
            .with_skip_from_title(),
        Handler::new("languages", r"\bHUN(?:garian)?\b")
            .with_transform(Transform::ValueSet("hu".to_string()))
            .with_keep_matching()
            .with_skip_from_title(),
        // Romanian language handlers (TS lines 2659-2671).
        Handler::new("languages", r"\bROM(?:anian)?\b")
            .with_transform(Transform::ValueSet("ro".to_string()))
            .with_keep_matching()
            .with_skip_from_title(),
        Handler::new("languages", r"\bRO(?:[ .,/-]*(?:[A-Z]{2}[ .,/-]+)*sub)")
            .with_transform(Transform::ValueSet("ro".to_string()))
            .with_keep_matching(),
        // Bulgarian language handlers (TS lines 2674-2686).
        Handler::new("languages", r"\bbul(?:garian)?\b")
            .with_transform(Transform::ValueSet("bg".to_string()))
            .with_keep_matching()
            .with_skip_from_title(),
        Handler::new("languages", r"\bBGAUDIO\b")
            .with_transform(Transform::ValueSet("bg".to_string()))
            .with_keep_matching()
            .with_remove()
            .with_skip_from_title(),
        // Serbian/Croatian/Slovenian language handlers (TS lines 2691-2714).
        Handler::new("languages", r"\b(?:srp|serbian)\b")
            .with_transform(Transform::ValueSet("sr".to_string()))
            .with_keep_matching(),
        Handler::new("languages", r"\b(?:HRV|croatian)\b")
            .with_transform(Transform::ValueSet("hr".to_string()))
            .with_keep_matching(),
        Handler::new(
            "languages",
            r"\bHR(?:[ .,/-]*(?:[A-Z]{2}[ .,/-]+)*sub\w*)\b",
        )
        .with_transform(Transform::ValueSet("hr".to_string()))
        .with_keep_matching(),
        Handler::new("languages", r"\bslovenian\b")
            .with_transform(Transform::ValueSet("sl".to_string()))
            .with_keep_matching()
            .with_skip_from_title(),
        // Dutch language handlers (TS lines 2718-2736).
        Handler::new("languages", r"\b(?:(?:w{3}\.\w+\.)?NL|dut|holand[eê]s)\b")
            .with_validator(Validator::NotMatch(
                crate::js_regex::compile_ci(r"(?:w{3}\.\w+\.)NL")
                    .unwrap_or_else(crate::js_regex::never_match),
            ))
            .with_transform(Transform::ValueSet("nl".to_string()))
            .with_keep_matching(),
        Handler::new("languages", r"\bdutch\b")
            .with_transform(Transform::ValueSet("nl".to_string()))
            .with_keep_matching()
            .with_skip_from_title(),
        Handler::new("languages", r"\bflemish\b")
            .with_transform(Transform::ValueSet("nl".to_string()))
            .with_keep_matching(),
        // Danish language handlers (TS lines 2740-2758).
        Handler::new("languages", r"\b(?:DK|danska|dansub|nordic)\b")
            .with_transform(Transform::ValueSet("da".to_string()))
            .with_keep_matching(),
        Handler::new("languages", r"\b(danish|dinamarqu[eê]s)\b")
            .with_transform(Transform::ValueSet("da".to_string()))
            .with_keep_matching()
            .with_skip_from_title(),
        Handler::new("languages", r"\bdan\b(?:.*\.(?:srt|vtt|ssa|ass|sub|idx)$)")
            .with_transform(Transform::ValueSet("da".to_string()))
            .with_keep_matching()
            .with_skip_from_title(),
        // Finnish language handlers (TS lines 2762-2776).
        Handler::new(
            "languages",
            r"\b(?:(?:w{3}\.\w+\.|Sci-)?FI|finsk|finsub|nordic)\b",
        )
        .with_validator(Validator::NotMatch(
            crate::js_regex::compile_ci(r"(?:w{3}\.\w+\.|Sci-)FI")
                .unwrap_or_else(crate::js_regex::never_match),
        ))
        .with_transform(Transform::ValueSet("fi".to_string()))
        .with_keep_matching(),
        Handler::new("languages", r"\bfinnish\b")
            .with_transform(Transform::ValueSet("fi".to_string()))
            .with_keep_matching()
            .with_skip_from_title(),
        // Swedish language handlers (TS lines 2778-2792).
        Handler::new(
            "languages",
            r"\b(?:(?:w{3}\.\w+\.)?SE|swe|swesubs?|sv(?:ensk)?|nordic)\b",
        )
        .with_validator(Validator::NotMatch(
            crate::js_regex::compile_ci(r"(?:w{3}\.\w+\.)SE")
                .unwrap_or_else(crate::js_regex::never_match),
        ))
        .with_transform(Transform::ValueSet("sv".to_string()))
        .with_keep_matching(),
        Handler::new("languages", r"\b(swedish|sueco)\b")
            .with_transform(Transform::ValueSet("sv".to_string()))
            .with_keep_matching()
            .with_skip_from_title(),
        // Norwegian language handlers (TS lines 2794-2806).
        Handler::new("languages", r"\b(?:NOR|norsk|norsub|nordic)\b")
            .with_transform(Transform::ValueSet("no".to_string()))
            .with_keep_matching(),
        Handler::new(
            "languages",
            r"\b(norwegian|noruegu[eê]s|bokm[aå]l|nob|nor(?:[\]_)]?\.\w{2,4}$))\b",
        )
        .with_transform(Transform::ValueSet("no".to_string()))
        .with_keep_matching()
        .with_skip_from_title(),
        // Arabic language handlers (TS lines 2810-2831).
        Handler::new("languages", r"\b(?:arabic|[aá]rabe|ara)\b")
            .with_transform(Transform::ValueSet("ar".to_string()))
            .with_keep_matching()
            .with_skip_if_first(),
        Handler::new(
            "languages",
            r"\barab.*(?:audio|lang(?:uage)?|sub(?:s|titles?)?)\b",
        )
        .with_transform(Transform::ValueSet("ar".to_string()))
        .with_keep_matching()
        .with_skip_from_title(),
        Handler::new("languages", r"\bar(?:\.(?:ass|ssa|srt|sub|idx)$)")
            .with_transform(Transform::ValueSet("ar".to_string()))
            .with_keep_matching()
            .with_skip_from_title(),
        // Turkish language handlers (TS lines 2833-2845).
        Handler::new("languages", r"\b(?:turkish|tur(?:co)?)\b")
            .with_transform(Transform::ValueSet("tr".to_string()))
            .with_keep_matching()
            .with_skip_from_title(),
        Handler::new(
            "languages",
            r"\b(TİVİBU|tivibu|bitturk(?:\.net)?|turktorrent)\b",
        )
        .with_transform(Transform::ValueSet("tr".to_string()))
        .with_keep_matching()
        .with_skip_from_title(),
        // Vietnamese language handlers (TS lines 2849-2856).
        Handler::new("languages", r"\bvietnamese\b|\bvie(?:[\]_)]?\.\w{2,4}$)")
            .with_transform(Transform::ValueSet("vi".to_string()))
            .with_keep_matching()
            .with_skip_from_title(),
        // Indonesian language handlers (TS lines 2858-2865).
        Handler::new("languages", r"\bind(?:onesian)?\b")
            .with_transform(Transform::ValueSet("id".to_string()))
            .with_keep_matching()
            .with_skip_from_title(),
        // Thai language handlers (TS lines 2867-2881).
        Handler::new("languages", r"\b(thai|tailand[eê]s)\b")
            .with_transform(Transform::ValueSet("th".to_string()))
            .with_keep_matching()
            .with_skip_if_first(),
        Handler::new_case_sensitive("languages", r"\b(THA|tha)\b")
            .with_transform(Transform::ValueSet("th".to_string()))
            .with_keep_matching()
            .with_skip_from_title(),
        // Malay language handlers (TS lines 2883-2890).
        Handler::new(
            "languages",
            r"\b(?:malay|may(?:[\]_)]?\.\w{2,4}$)|(?:subs?\([a-z,]+)may)\b",
        )
        .with_transform(Transform::ValueSet("ms".to_string()))
        .with_keep_matching(),
        // Hebrew language handlers (TS lines 2891-2898).
        Handler::new("languages", r"\bheb(?:rew|raico)?\b")
            .with_transform(Transform::ValueSet("he".to_string()))
            .with_keep_matching()
            .with_skip_from_title(),
        // Persian language handlers (TS lines 2900-2907).
        Handler::new("languages", r"\b(persian|persa)\b")
            .with_transform(Transform::ValueSet("fa".to_string()))
            .with_keep_matching()
            .with_skip_from_title(),
        // Unicode spt detection for languages (TS lines 2909-3007).
        Handler::new("languages", r"[\u3040-\u30ff]+")
            .with_transform(Transform::ValueSet("ja".to_string()))
            .with_keep_matching()
            .with_skip_from_title(),
        Handler::new("languages", r"[\u3400-\u4dbf]+")
            .with_transform(Transform::ValueSet("zh".to_string()))
            .with_keep_matching()
            .with_skip_from_title(),
        Handler::new("languages", r"[\u4e00-\u9fff]+")
            .with_transform(Transform::ValueSet("zh".to_string()))
            .with_keep_matching()
            .with_skip_from_title(),
        Handler::new("languages", r"[\uf900-\ufaff]+")
            .with_transform(Transform::ValueSet("zh".to_string()))
            .with_keep_matching()
            .with_skip_from_title(),
        Handler::new("languages", r"[\uff66-\uff9f]+")
            .with_transform(Transform::ValueSet("ja".to_string()))
            .with_keep_matching()
            .with_skip_from_title(),
        Handler::new("languages", r"[\u0400-\u04ff]+")
            .with_transform(Transform::ValueSet("ru".to_string()))
            .with_keep_matching()
            .with_skip_from_title(),
        Handler::new("languages", r"[\u0600-\u06ff]+")
            .with_transform(Transform::ValueSet("ar".to_string()))
            .with_keep_matching()
            .with_skip_from_title(),
        Handler::new("languages", r"[\u0750-\u077f]+")
            .with_transform(Transform::ValueSet("ar".to_string()))
            .with_keep_matching()
            .with_skip_from_title(),
        Handler::new("languages", r"[\u0c80-\u0cff]+")
            .with_transform(Transform::ValueSet("kn".to_string()))
            .with_keep_matching()
            .with_skip_from_title(),
        Handler::new("languages", r"[\u0d00-\u0d7f]+")
            .with_transform(Transform::ValueSet("ml".to_string()))
            .with_keep_matching()
            .with_skip_from_title(),
        Handler::new("languages", r"[\u0e00-\u0e7f]+")
            .with_transform(Transform::ValueSet("th".to_string()))
            .with_keep_matching()
            .with_skip_from_title(),
        Handler::new("languages", r"[\u0900-\u097f]+")
            .with_transform(Transform::ValueSet("hi".to_string()))
            .with_keep_matching()
            .with_skip_from_title(),
        Handler::new("languages", r"[\u0980-\u09ff]+")
            .with_transform(Transform::ValueSet("bn".to_string()))
            .with_keep_matching()
            .with_skip_from_title(),
        Handler::new("languages", r"[\u0a00-\u0a7f]+")
            .with_transform(Transform::ValueSet("gu".to_string()))
            .with_keep_matching()
            .with_skip_from_title(),
    ]
}

#[cfg(test)]
mod tests {
    use crate::parse_torrent_title;

    /// Cases taken from the TypeScript `languages.test.ts` corpus; only the
    /// `languages` field (this file's field) is asserted.
    #[test]
    fn languages_table_cases() {
        let cases: &[(&str, Option<Vec<&str>>)] = &[
            (
                "Shinjuku Swan 2015 JAP 1080p BluRay x264 DTS-JYK",
                Some(vec!["ja"]),
            ),
            (
                "The Intern 2015 TRUEFRENCH 720p BluRay x264-PiNKPANTERS",
                Some(vec!["fr"]),
            ),
            ("Dilbert complete series + en subs", Some(vec!["en"])),
            ("Traditional Chinese.chi.srt", Some(vec!["zh-tw"])),
            (
                "Mary Poppins Returns 2019 DVDRip LATINO-1XBET",
                Some(vec!["es-419"]),
            ),
            ("Carros 2 Dublado - Portugues BR (2011)", Some(vec!["pt"])),
            (
                "Quarantine [2008] [DVDRiP.XviD-M14CH0] [Lektor PL] [Arx]",
                Some(vec!["pl"]),
            ),
            (
                "Frieren - Beyond Journey's End - S01E01 - TBA WEBDL-1080p.de.ass",
                Some(vec!["de"]),
            ),
            (
                "[NC-Raws] 叫我對大哥 (WEB版) / Ore, Tsushima - 10 [Baha][WEB-DL][1080p][AVC AAC]\
                 [CHT][MP4]",
                Some(vec!["zh"]),
            ),
            // `skipIfFirst` keeps a title-word "Greek" from reading as the
            // language when the year already matched later in the title.
            (
                "My Big Fat Greek Wedding (2002) 720p BrRip x264 - YIFY",
                None,
            ),
        ];
        for (title, expected) in cases {
            let parsed = parse_torrent_title(title);
            let expected: Option<Vec<String>> = expected
                .as_ref()
                .map(|v| v.iter().map(std::string::ToString::to_string).collect());
            assert_eq!(parsed.languages, expected, "title: {title}");
        }
    }
}
