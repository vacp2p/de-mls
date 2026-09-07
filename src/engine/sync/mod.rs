//! `ConversationSync`: the pull side (`request`), the answer side
//! (`answer`), validation and adoption (`adopt`), and the steward-list
//! housekeeping the sync carries (`steward_list`). Design §14.

mod adopt;
mod answer;
mod request;
mod steward_list;

#[cfg(test)]
mod tests;
