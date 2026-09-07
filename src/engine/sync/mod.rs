//! `ConversationSync`: the pull side (`request`), the answer side
//! (`answer`), validation and adoption (`adopt`). Design §14.

mod adopt;
mod answer;
mod request;
mod steward_list;

#[cfg(test)]
mod tests;
