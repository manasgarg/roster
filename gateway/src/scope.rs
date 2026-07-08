//! Namespaced scope matching, shared by the judge (rules) and the ledger
//! (limits). A scope governs a subject if the subject is that scope or nests
//! under it — so "org" governs the whole fleet and "org/yuko" one worker.

pub fn applies(scope: &str, subject: &str) -> bool {
    subject == scope || subject.starts_with(&format!("{scope}/"))
}
