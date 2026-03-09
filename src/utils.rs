#[derive(Debug)]
pub(crate) struct SnipTextCtx {
    pub(crate) bytes: usize,
    pub(crate) max_bytes: usize,
}

pub(crate) fn snip_long_text(
    output: String,
    max_bytes: usize,
    snip_message_fmt: impl Fn(SnipTextCtx) -> String,
) -> String {
    if output.len() <= max_bytes {
        return output;
    }

    let snip_msg = snip_message_fmt(SnipTextCtx {
        bytes: output.len(),
        max_bytes,
    });
    let mut truncated = output;

    let mut byte_limit = max_bytes.saturating_sub(snip_msg.len());

    while !truncated.is_char_boundary(byte_limit) && byte_limit > 0 {
        byte_limit -= 1;
    }

    truncated.truncate(byte_limit);
    truncated.push_str(&snip_msg);
    truncated
}
