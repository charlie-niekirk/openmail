//! How the three panes follow each other: the mailbox selected in the
//! sidebar picks what the thread list shows, and the thread selected in the
//! list opens in the reading pane. Plain functions from a pane's event to
//! the commands for the others, so the wiring is tested without a window;
//! [`MainWindow`](crate::MainWindow) carries the commands out.

use openmail_core::ThreadId;
use openmail_store::ThreadView;

use crate::sidebar::Mailbox;
use crate::sidebar_view::SidebarEvent;
use crate::thread_list_view::ThreadListEvent;

/// What a selection in one pane asks of the others, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaneCommand {
    /// Show these threads in the thread list, from the top, with nothing
    /// selected.
    ShowThreads(ThreadView),
    /// Open this thread in the reading pane.
    OpenThread(ThreadId),
    /// Close the reading pane's thread.
    CloseThread,
}

/// The threads the list shows for a mailbox.
pub fn thread_view(mailbox: &Mailbox) -> ThreadView {
    match mailbox {
        Mailbox::Label(id) => ThreadView::Label(id.clone()),
        Mailbox::AllMail => ThreadView::AllMail,
    }
}

/// What the other panes do when the sidebar's selection changes: the list
/// shows the new mailbox, and the thread that was open (from the old one)
/// closes, as nothing in the new list is selected yet.
pub fn on_sidebar(event: &SidebarEvent) -> Vec<PaneCommand> {
    match event {
        SidebarEvent::Selected(mailbox) => vec![
            PaneCommand::ShowThreads(thread_view(mailbox)),
            PaneCommand::CloseThread,
        ],
    }
}

/// What the other panes do when the thread list's selection changes.
pub fn on_thread_list(event: &ThreadListEvent) -> Vec<PaneCommand> {
    match event {
        ThreadListEvent::Selected(id) => vec![PaneCommand::OpenThread(id.clone())],
    }
}

#[cfg(test)]
mod tests {
    use openmail_core::{LabelId, SystemLabel};

    use super::*;
    use crate::thread_list::inbox;

    #[test]
    fn mailboxes_map_to_thread_views() {
        assert_eq!(thread_view(&Mailbox::inbox()), inbox());
        assert_eq!(thread_view(&Mailbox::AllMail), ThreadView::AllMail);
        let work = LabelId::new("Label_7");
        assert_eq!(
            thread_view(&Mailbox::Label(work.clone())),
            ThreadView::Label(work)
        );
        // Both panes start on the Inbox, so they agree before any click.
        assert_eq!(thread_view(&Mailbox::default()), inbox());
    }

    #[test]
    fn selecting_a_mailbox_filters_the_list_and_closes_the_thread() {
        let sent = SystemLabel::Sent.id();
        assert_eq!(
            on_sidebar(&SidebarEvent::Selected(Mailbox::Label(sent.clone()))),
            [
                PaneCommand::ShowThreads(ThreadView::Label(sent)),
                PaneCommand::CloseThread,
            ]
        );
        assert_eq!(
            on_sidebar(&SidebarEvent::Selected(Mailbox::AllMail)),
            [
                PaneCommand::ShowThreads(ThreadView::AllMail),
                PaneCommand::CloseThread,
            ]
        );
    }

    #[test]
    fn selecting_a_thread_opens_it() {
        let id = ThreadId::new("t1");
        assert_eq!(
            on_thread_list(&ThreadListEvent::Selected(id.clone())),
            [PaneCommand::OpenThread(id)]
        );
    }
}
