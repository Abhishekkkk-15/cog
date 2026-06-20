/// Rough token estimate used only to trigger context-budget summarization,
/// not for billing — chars/4 is the standard cheap approximation.
pub fn estimate_tokens(s: &str) -> usize {
    s.len() / 4
}
