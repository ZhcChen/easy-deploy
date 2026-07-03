use encoding_rs::GBK;

pub fn fix_mojibake(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.is_empty() || !looks_like_mojibake(trimmed) {
        return value.to_owned();
    }

    let replaced = replace_known_tokens(value);
    if replaced != value {
        return replaced;
    }

    let (encoded, _, had_errors) = GBK.encode(value);
    if had_errors {
        return value.to_owned();
    }

    match String::from_utf8(encoded.into_owned()) {
        Ok(candidate) if looks_better_after_fix(value, &candidate) => candidate,
        _ => value.to_owned(),
    }
}

fn looks_like_mojibake(value: &str) -> bool {
    const BAD_MARKERS: [&str; 16] = [
        "鍦", "绂", "宸", "鏈", "鍙", "鎺", "娴", "纾", "閼", "鑺", "鏆", "鍚", "瀹", "鏌", "闃",
        "甯",
    ];
    BAD_MARKERS.iter().any(|marker| value.contains(marker))
}

fn looks_better_after_fix(original: &str, candidate: &str) -> bool {
    if candidate.trim().is_empty() || candidate == original {
        return false;
    }

    let original_bad = mojibake_score(original);
    let candidate_bad = mojibake_score(candidate);
    let candidate_good = readable_score(candidate);

    candidate_bad < original_bad && candidate_good > 0
}

fn replace_known_tokens(value: &str) -> String {
    let mut output = value.to_owned();
    for (from, to) in KNOWN_MOJIBAKE_REPLACEMENTS {
        output = output.replace(from, to);
    }
    output
}

const KNOWN_MOJIBAKE_REPLACEMENTS: &[(&str, &str)] = &[
    ("鍦ㄧ嚎", "在线"),
    ("绂荤嚎", "离线"),
    ("宸茬鐢", "已禁用"),
    ("鏈帰娴", "未探测"),
    ("鏈褰", "未记录"),
    ("鏈儴缃", "未部署"),
    ("鏆傛棤杩愯淇℃伅", "暂无运行信息"),
    ("鏈叧鑱斿簲鐢", "未关联应用"),
    ("鏆傛棤鎽樿", "暂无摘要"),
    ("鏈湴鎵ц", "本地执行"),
    ("鏈満", "本机"),
    ("鏈煡绫诲瀷", "未知类型"),
    ("鏈垎鍖", "未分区"),
    ("鏈缃", "未设置"),
    ("灏氭湭鎺㈡祴", "尚未探测"),
    ("绛夊緟鑺傜偣鎺㈡祴", "等待节点探测"),
    ("鑺傜偣", "节点"),
    ("鎺㈡祴", "探测"),
    ("閫氳繃", "通过"),
    ("鍙敤", "可用"),
    ("鍏抽棴", "关闭"),
    ("鍚敤", "启用"),
    ("绂佺敤", "禁用"),
    ("涓?", "与"),
    ("锛孌ocker", "，Docker"),
    ("与Compose", "与 Compose"),
    ("OS 鏈帰娴", "OS 未探测"),
    ("纾佺洏鏈帰娴", "磁盘未探测"),
    ("systemd 鏈帰娴", "systemd 未探测"),
];

fn mojibake_score(value: &str) -> usize {
    const BAD_MARKERS: [&str; 16] = [
        "鍦", "绂", "宸", "鏈", "鍙", "鎺", "娴", "纾", "閼", "鑺", "鏆", "鍚", "瀹", "鏌", "闃",
        "甯",
    ];
    BAD_MARKERS
        .iter()
        .map(|marker| value.matches(marker).count())
        .sum()
}

fn readable_score(value: &str) -> usize {
    value
        .chars()
        .filter(|ch| {
            ('\u{4e00}'..='\u{9fff}').contains(ch)
                || ch.is_ascii_alphanumeric()
                || matches!(
                    ch,
                    ' ' | ':' | '-' | '_' | '/' | '.' | ',' | '(' | ')' | '[' | ']' | '+' | '%'
                )
        })
        .count()
}

#[cfg(test)]
mod tests {
    use super::fix_mojibake;

    #[test]
    fn fixes_common_node_probe_mojibake() {
        assert_eq!(fix_mojibake("鍦ㄧ嚎"), "在线");
        assert_eq!(fix_mojibake("绂荤嚎"), "离线");
        assert_eq!(
            fix_mojibake("SSH 鑺傜偣鎺㈡祴閫氳繃锛孌ocker 涓?Compose 鍙敤"),
            "SSH 节点探测通过，Docker 与 Compose 可用"
        );
    }

    #[test]
    fn keeps_normal_text_unchanged() {
        assert_eq!(
            fix_mojibake("Docker version 26.1.0"),
            "Docker version 26.1.0"
        );
        assert_eq!(fix_mojibake("在线"), "在线");
    }
}
