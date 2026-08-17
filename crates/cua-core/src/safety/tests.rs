#[cfg(test)]
mod tests {
    use super::*;

    fn button(label: &str) -> Candidate {
        Candidate {
            role: "AXButton".to_string(),
            label: Some(label.to_string()),
            description: format!("[1] AXButton {label:?}"),
            ..Candidate::default()
        }
    }

    fn field(value: &str) -> Candidate {
        Candidate {
            role: "AXTextField".to_string(),
            label: Some("Message".to_string()),
            value: Some(value.to_string()),
            settable: true,
            description: "[2] AXTextField".to_string(),
            ..Candidate::default()
        }
    }

    // ── blocklist ────────────────────────────────────────────────────────

    #[test]
    fn the_blocklist_matches_apples_credential_stores() {
        assert!(forbidden_bundle("com.apple.keychainaccess").is_some());
        assert!(forbidden_bundle("com.apple.Passwords").is_some());
    }

    #[test]
    fn the_blocklist_matches_third_party_password_managers() {
        for id in [
            "com.1password.1password",
            "com.agilebits.onepassword7",
            "com.bitwarden.desktop",
            "com.lastpass.LastPass",
            "org.keepassxc.keepassxc",
            "me.proton.pass.electron",
        ] {
            assert!(forbidden_bundle(id).is_some(), "{id} should be forbidden");
        }
    }

    #[test]
    fn the_blocklist_covers_helper_processes_of_a_blocked_app() {
        // A password manager's helper can hold the same window a click would
        // land in, so an entry has to cover the family, not one process.
        assert!(forbidden_bundle("com.1password.1password.helper").is_some());
        assert!(forbidden_bundle("com.apple.keychainaccess.SomeHelper").is_some());
    }

    #[test]
    fn the_blocklist_does_not_match_a_longer_unrelated_identifier() {
        // The `.` boundary is what stops a prefix rule from swallowing an
        // identifier that merely starts with the same characters.
        assert!(forbidden_bundle("com.apple.systempreferencesque").is_none());
        // …though a name-based backstop still fires when the id says
        // "keychain" anywhere, which is the deliberate over-reach documented on
        // `SUSPICIOUS`.
        assert!(forbidden_bundle("com.apple.keychainaccessorize").is_some());
    }

    #[test]
    fn the_blocklist_matches_security_surfaces_and_login_prompts() {
        assert_eq!(
            forbidden_bundle("com.apple.systempreferences"),
            Some(SECURITY_SURFACE)
        );
        assert_eq!(forbidden_bundle("com.apple.loginwindow"), Some(AUTH_PROMPT));
        assert_eq!(
            forbidden_bundle("com.apple.SecurityAgent"),
            Some(AUTH_PROMPT)
        );
    }

    #[test]
    fn an_unknown_password_manager_is_still_caught_by_its_bundle_id() {
        assert!(forbidden_bundle("com.example.SuperPasswordVault").is_some());
        assert!(forbidden_bundle("net.example.totp-authenticator").is_some());
    }

    #[test]
    fn ordinary_apps_are_not_forbidden() {
        for id in [
            "com.apple.TextEdit",
            "com.apple.Notes",
            "com.kakao.KakaoTalk",
            "com.tinyspeck.slackmacgap",
            "com.google.Chrome",
            "com.apple.Terminal",
        ] {
            assert!(forbidden_bundle(id).is_none(), "{id} should be drivable");
        }
    }

    #[test]
    fn matching_is_case_insensitive_and_ignores_surrounding_space() {
        assert!(forbidden_bundle("  COM.APPLE.KEYCHAINACCESS  ").is_some());
    }

    #[test]
    fn an_app_with_no_bundle_identifier_is_not_matched_by_accident() {
        assert!(forbidden_bundle("").is_none());
        assert!(forbidden_bundle("   ").is_none());
    }

    // ── destructive labels ───────────────────────────────────────────────

    #[test]
    fn plain_english_destructive_labels_are_caught() {
        for label in [
            "Delete",
            "Delete All",
            "Delete History",
            "Remove Account",
            "Erase All Content and Settings",
            "Discard Changes",
            "Reset",
            "Move to Trash",
            "Empty Trash",
            "Uninstall",
            "Revoke Access",
            "Don't Save",
            "Don’t Save",
            "Clear History",
            "Deleting…",
        ] {
            assert!(
                destructive_token(label).is_some(),
                "{label:?} should need confirmation"
            );
        }
    }

    #[test]
    fn korean_destructive_labels_are_caught() {
        for label in [
            "삭제",
            "모두 삭제",
            "채팅방 나가기",
            "대화 내용 삭제",
            "계정 제거",
            "설정 초기화",
            "휴지통으로 이동",
            "휴지통 비우기",
            "저장 안 함",
            "저장하지 않음",
            "회원 탈퇴",
        ] {
            assert!(
                destructive_token(label).is_some(),
                "{label:?} should need confirmation"
            );
        }
    }

    #[test]
    fn harmless_labels_are_not_caught() {
        for label in [
            "Cancel", "취소", "OK", "확인", "Save", "저장", "Send", "New Note", "Search", "Close",
            "닫기", "Reply", "Settings", "설정",
        ] {
            assert!(
                destructive_token(label).is_none(),
                "{label:?} must not need confirmation"
            );
        }
    }

    #[test]
    fn a_word_that_merely_contains_a_stem_is_not_destructive() {
        // The precision half of the heuristic. `Presets` contains `reset` and
        // `Undelete` contains `delete`, and neither one removes anything.
        assert!(destructive_token("Presets").is_none());
        assert!(destructive_token("Undelete").is_none());
        assert!(destructive_token("Preset Manager").is_none());
    }

    #[test]
    fn punctuation_and_wrapping_do_not_hide_a_destructive_label() {
        assert!(destructive_token("Delete…").is_some());
        assert!(destructive_token("(Delete)").is_some());
        assert!(destructive_token("Delete\n   All Messages").is_some());
        assert!(destructive_token("DELETE ALL").is_some());
    }

    #[test]
    fn an_empty_label_is_not_destructive() {
        assert!(destructive_token("").is_none());
        assert!(destructive_token("   ").is_none());
    }

    #[test]
    fn a_text_fields_own_contents_are_never_classified() {
        // Otherwise `set_value` on a note that happens to say "delete the old
        // files" would be refused, and no confirmation would make that sensible.
        let f = field("remind me to delete the old files");
        assert!(destructive_token(&f.classifiable_text()).is_none());
    }

    #[test]
    fn a_buttons_value_is_classified_because_it_is_part_of_what_it_says() {
        let mut c = button("");
        c.label = None;
        c.value = Some("Delete All".to_string());
        assert!(destructive_token(&c.classifiable_text()).is_some());
    }

    #[test]
    fn a_tooltip_counts_as_a_label() {
        let mut c = button("⌫");
        c.help = Some("Move this conversation to the trash".to_string());
        assert!(destructive_token(&c.classifiable_text()).is_some());
    }

    // ── the question a dialog is asking ──────────────────────────────────
    //
    // These build snapshot-shaped trees rather than asserting on strings,
    // because the rule being tested is about *shape*: which ancestor, how far
    // up, which text inside it. A string-level test would pass with the
    // ancestor rule deleted.

    /// A flat tree in the form the snapshot records one: parents before
    /// children, every node naming its parent by index.
    #[derive(Default)]
    struct Tree {
        nodes: Vec<ContextNode<'static>>,
    }

    impl Tree {
        fn push(&mut self, node: ContextNode<'static>) -> usize {
            self.nodes.push(node);
            self.nodes.len() - 1
        }

        /// An ordinary document window. Layout, never a question.
        fn window(&mut self, title: &'static str) -> usize {
            self.push(ContextNode {
                parent: None,
                role: "AXWindow",
                subrole: Some("AXStandardWindow"),
                label: Some(title),
                ..ContextNode::default()
            })
        }

        /// What AppKit publishes for a free-standing `NSAlert`: an ordinary
        /// window role carrying the dialog subrole.
        fn dialog_window(&mut self, title: &'static str) -> usize {
            self.push(ContextNode {
                parent: None,
                role: "AXWindow",
                subrole: Some("AXDialog"),
                label: Some(title),
                ..ContextNode::default()
            })
        }

        /// The document-modal form: a sheet attached to a window.
        fn sheet(&mut self, parent: usize, title: Option<&'static str>) -> usize {
            self.push(ContextNode {
                parent: Some(parent),
                role: "AXSheet",
                label: title,
                ..ContextNode::default()
            })
        }

        fn group(&mut self, parent: usize) -> usize {
            self.push(ContextNode {
                parent: Some(parent),
                role: "AXGroup",
                ..ContextNode::default()
            })
        }

        fn container(&mut self, parent: usize, role: &'static str) -> usize {
            self.push(ContextNode {
                parent: Some(parent),
                role,
                ..ContextNode::default()
            })
        }

        fn text(&mut self, parent: usize, body: &'static str) -> usize {
            self.push(ContextNode {
                parent: Some(parent),
                role: "AXStaticText",
                label: Some(body),
                ..ContextNode::default()
            })
        }

        fn button(&mut self, parent: usize, label: &'static str) -> usize {
            self.push(ContextNode {
                parent: Some(parent),
                role: "AXButton",
                label: Some(label),
                ..ContextNode::default()
            })
        }

        fn field(&mut self, parent: usize, value: &'static str) -> usize {
            self.push(ContextNode {
                parent: Some(parent),
                role: "AXTextField",
                label: Some("Name"),
                value: Some(value),
                settable: true,
                ..ContextNode::default()
            })
        }

        /// The candidate `session::safety_candidate` would hand the gate for
        /// this node — same fields, same context lookup.
        fn candidate(&self, index: usize) -> Candidate {
            let n = self.nodes[index];
            Candidate {
                role: n.role.to_string(),
                label: n.label.map(str::to_string),
                value: n.value.map(str::to_string),
                help: n.help.map(str::to_string),
                settable: n.settable,
                caption: caption(&self.nodes, index),
                description: format!("[{index}] {} {:?}", n.role, n.label.unwrap_or_default()),
                context: decision_context(&self.nodes, index),
            }
        }

        /// What `guard` would conclude about this node, in the same order:
        /// the control's own words first, then the question it answers.
        fn verdict(&self, index: usize) -> Option<String> {
            let c = self.candidate(index);
            destructive_token(&c.classifiable_text())
                .or_else(|| destructive_context(&c).map(|(_, matched)| matched))
        }
    }

    /// The commonest destructive shape on macOS: a terse button under a
    /// sheet whose text carries the whole meaning.
    fn confirm_sheet(
        message: &'static str,
        informative: &'static str,
        answers: &[&'static str],
    ) -> (Tree, Vec<usize>) {
        let mut t = Tree::default();
        let window = t.window("Documents");
        let sheet = t.sheet(window, None);
        let body = t.group(sheet);
        t.text(body, message);
        t.text(body, informative);
        let answers = answers.iter().map(|a| t.button(sheet, a)).collect();
        (t, answers)
    }

    #[test]
    fn a_terse_button_inherits_the_question_its_sheet_is_asking() {
        let (t, answers) = confirm_sheet(
            "Delete 4 items?",
            "This action cannot be undone.",
            &["OK", "Cancel"],
        );
        assert_eq!(t.verdict(answers[0]).as_deref(), Some("delet"));
    }

    #[test]
    fn the_korean_form_of_the_same_dialog_is_caught_too() {
        // 확인 says nothing on its own, exactly like OK, and the maintainer's
        // apps are Korean.
        let (t, answers) = confirm_sheet(
            "4개 항목을 삭제할까요?",
            "이 동작은 되돌릴 수 없습니다.",
            &["확인", "취소"],
        );
        assert_eq!(t.verdict(answers[0]).as_deref(), Some("삭제"));
    }

    #[test]
    fn cancelling_a_destructive_dialog_is_never_refused() {
        // The load-bearing exemption. Cancel is how a caller *avoids* the
        // destruction on offer; refusing it would leave an agent stuck in a
        // modal sheet whose only exit is to send confirm_destructive: true,
        // which is the habit that would make this gate meaningless everywhere
        // else.
        let (t, answers) = confirm_sheet(
            "Delete 4 items?",
            "This action cannot be undone.",
            &["OK", "Cancel"],
        );
        assert_eq!(t.verdict(answers[1]), None);

        let (t, answers) = confirm_sheet(
            "4개 항목을 삭제할까요?",
            "이 동작은 되돌릴 수 없습니다.",
            &["확인", "취소"],
        );
        assert_eq!(t.verdict(answers[1]), None);
    }

    #[test]
    fn a_dismissing_answer_is_matched_whole_and_never_as_a_substring() {
        for yes in [
            "Cancel",
            "cancel",
            "Cancel…",
            "(Cancel)",
            "No",
            "Not now",
            "취소",
            "유지",
            "Keep",
            "Save",
            "저장",
        ] {
            assert!(is_safe_answer(yes), "{yes:?} names its own harmlessness");
        }
        // The direction that matters: a destructive control must not be
        // excused by containing a soft word, and an answer that promises
        // nothing — OK, 확인, Continue — is not on the list at all.
        for no in [
            "Close Account",
            "No Backup, Delete",
            "Cancel Subscription",
            "Keep Nothing",
            "취소선 삭제",
            "Don't Save",
            "저장 안 함",
            "Save and Delete",
            "Replace",
            "OK",
            "확인",
            "Continue",
            "",
        ] {
            assert!(!is_safe_answer(no), "{no:?} promises nothing");
        }
    }

    #[test]
    fn the_save_sheet_macos_actually_ships_is_handled_end_to_end() {
        // Transcribed from a live read of TextEdit's close-without-saving sheet
        // on a Korean system, identifiers and all. Three things have to be true
        // at once here, and only the middle one is obvious:
        //
        //   저장 (Save)  — allowed. The sheet is a destructive question, but
        //                  the answer that preserves the work must not be
        //                  gated, or every close-without-saving flow trains a
        //                  caller to confirm reflexively.
        //   삭제 (Delete)— refused, on its own label, before context is asked.
        //   취소 (Cancel)— allowed, the way out.
        let mut t = Tree::default();
        let window = t.window("무제");
        let sheet = t.sheet(window, Some("저장"));
        // A real static text carries its sentence in the value and an internal
        // identifier in the title.
        for (identifier, sentence) in [
            ("whereLabel", "위치:"),
            ("_NS:246", "이 새로운 문서(‘무제’)를 유지하겠습니까?"),
            (
                "_NS:239",
                "변경 사항을 저장하거나 이 문서를 즉시 삭제할 수도 있습니다. 이 동작은 취소할 수 \
                 없습니다.",
            ),
            ("fileFormatLabel", "파일 포맷:"),
        ] {
            t.push(ContextNode {
                parent: Some(sheet),
                role: "AXStaticText",
                label: Some(identifier),
                value: Some(sentence),
                ..ContextNode::default()
            });
        }
        let delete = t.button(sheet, "삭제");
        let cancel = t.button(sheet, "취소");
        let save = t.button(sheet, "저장");

        assert_eq!(t.verdict(delete).as_deref(), Some("삭제"));
        assert_eq!(t.verdict(cancel), None);
        assert_eq!(t.verdict(save), None);

        // And the question a human would read in the refusal is the sentences,
        // not the identifiers around them.
        let question = t.candidate(delete).context.unwrap().question;
        assert!(question.contains("유지하겠습니까?"), "{question}");
        for identifier in ["whereLabel", "_NS:246", "fileFormatLabel"] {
            assert!(
                !question.contains(identifier),
                "{identifier} is plumbing, not prose: {question}"
            );
        }
    }

    #[test]
    fn a_sheet_that_erases_a_disk_catches_continue_as_well_as_ok() {
        let (t, answers) = confirm_sheet(
            "Are you sure?",
            "Erasing will permanently remove all data on “Backup”.",
            &["Continue", "Cancel"],
        );
        assert!(t.verdict(answers[0]).is_some());
        assert_eq!(t.verdict(answers[1]), None);
    }

    #[test]
    fn an_alert_window_is_a_question_even_though_its_role_says_window() {
        // AppKit's free-standing NSAlert: role AXWindow, subrole AXDialog. If
        // the rule keyed on role alone this shape would sail through.
        let mut t = Tree::default();
        let alert = t.dialog_window("");
        t.text(alert, "Delete “Report.pdf”?");
        let ok = t.button(alert, "OK");
        assert!(t.verdict(ok).is_some());
        assert_eq!(
            t.candidate(ok).context.unwrap().kind,
            "AXWindow[AXDialog]".to_string()
        );
    }

    #[test]
    fn an_ordinary_window_full_of_the_word_delete_is_not_a_question() {
        // The failure mode that made this feature hard: a mail window whose
        // content is a thread about deleting an account, a chat window whose
        // history says 삭제 twenty times. None of it is a decision context, so
        // none of it is evidence, at any depth.
        let mut t = Tree::default();
        let window = t.window("Re: please delete my account");
        let scroll = t.container(window, "AXScrollArea");
        let group = t.group(scroll);
        t.text(group, "Can you delete the old backups and erase the disk?");
        let reply = t.button(group, "Reply");
        let send = t.button(window, "Send");

        assert_eq!(t.verdict(reply), None);
        assert_eq!(t.verdict(send), None);
        assert!(t.candidate(reply).context.is_none());
    }

    #[test]
    fn a_window_title_alone_is_not_a_question() {
        // Deliberate: an ordinary window is layout even when its title reads
        // like an alert. The dialog subrole is what distinguishes them, and a
        // toolkit that publishes neither gets the benefit of the doubt rather
        // than making every button in the app confirmable.
        let mut t = Tree::default();
        let window = t.window("Delete 4 items?");
        let ok = t.button(window, "OK");
        assert_eq!(t.verdict(ok), None);
    }

    #[test]
    fn the_answers_are_not_part_of_the_question() {
        // An alert offering Delete and Cancel must not make Cancel — or any
        // other sibling — destructive by association. Only the prose counts.
        let (t, answers) = confirm_sheet(
            "Are you sure?",
            "You can change this later.",
            &["Delete", "Cancel", "More Info"],
        );
        assert_eq!(t.verdict(answers[0]).as_deref(), Some("delet")); // its own label
        assert_eq!(t.verdict(answers[1]), None);
        assert_eq!(t.verdict(answers[2]), None);

        // The same alert as a toolkit that renders each button as a container
        // with its caption inside it — Chromium and the Electron apps built on
        // it publish exactly this. If the walk descended into answers, the
        // Delete button's caption would become part of the question and Cancel
        // would be refused for standing next to it.
        let mut t = Tree::default();
        let alert = t.dialog_window("");
        t.text(alert, "Are you sure?");
        let delete = t.button(alert, "");
        t.text(delete, "Delete");
        let cancel = t.button(alert, "");
        t.text(cancel, "Cancel");

        assert_eq!(
            t.candidate(cancel).context.unwrap().question,
            "Are you sure?".to_string()
        );
        assert_eq!(t.verdict(cancel), None);
        // …and the button beside it is still caught, by its own caption rather
        // than by the question. A control that says "Delete" on screen must not
        // read as unlabeled just because the toolkit put the word in a child.
        assert_eq!(t.verdict(delete).as_deref(), Some("delet"));
    }

    #[test]
    fn a_caption_is_read_from_a_control_and_never_from_a_row() {
        // The boundary that keeps the caption rule from becoming a content
        // reader: a button's children are its wording, a row's children are the
        // user's mail.
        let mut t = Tree::default();
        let window = t.window("Mail");
        let table = t.container(window, "AXTable");
        let row = t.container(table, "AXRow");
        t.text(row, "Please delete my account");
        assert_eq!(caption(&t.nodes, row), None);
        assert_eq!(t.verdict(row), None);

        let button = t.button(window, "");
        t.text(button, "Empty Trash");
        assert_eq!(caption(&t.nodes, button).as_deref(), Some("Empty Trash"));
        assert!(t.verdict(button).is_some());
    }

    #[test]
    fn content_inside_a_dialog_is_still_content() {
        // A "Move to…" sheet listing the user's own files. The sheet is a
        // decision context, but a table of file names is not its question, and
        // a folder called "delete me" must not confirm-gate the Move button.
        let mut t = Tree::default();
        let window = t.window("Documents");
        let sheet = t.sheet(window, Some("Move 3 items to:"));
        let table = t.container(sheet, "AXTable");
        let row = t.container(table, "AXRow");
        t.text(row, "delete me");
        let move_button = t.button(sheet, "Move");

        assert_eq!(t.verdict(move_button), None);
        assert_eq!(
            t.candidate(move_button).context.unwrap().question,
            "Move 3 items to:".to_string()
        );
    }

    #[test]
    fn a_text_field_inside_a_destructive_dialog_is_still_writable() {
        // Two exclusions at once, and both have to hold. The field's own value
        // is never classified, and the question around it is not held against
        // typing either: the decision is the button underneath, not the name
        // being typed into the sheet.
        let mut t = Tree::default();
        let window = t.window("Documents");
        let sheet = t.sheet(window, None);
        t.text(sheet, "Deleting this project will remove 42 files.");
        let name = t.field(sheet, "delete the old files first");
        assert_eq!(t.verdict(name), None);
        assert!(
            t.candidate(name).context.is_some(),
            "the sheet is still a question — the field is just exempt from being asked it"
        );
    }

    #[test]
    fn a_writable_label_inside_a_dialog_never_becomes_the_question() {
        // The same rule one level out: the walk reads static text, but a
        // settable value inside the sheet is the user's own typing.
        let mut t = Tree::default();
        let window = t.window("Documents");
        let sheet = t.sheet(window, Some("Rename item"));
        t.push(ContextNode {
            parent: Some(sheet),
            role: "AXStaticText",
            value: Some("delete me"),
            settable: true,
            ..ContextNode::default()
        });
        let ok = t.button(sheet, "OK");
        assert_eq!(t.verdict(ok), None);
    }

    #[test]
    fn the_nearest_question_is_the_one_being_answered() {
        // A confirmation raised on top of a destructive dialog. The inner
        // sheet is what the button answers; inheriting the outer one would
        // make "OK" on "Rename this file?" a deletion.
        let mut t = Tree::default();
        let outer = t.dialog_window("");
        t.text(outer, "Erase “Macintosh HD”?");
        let inner = t.sheet(outer, None);
        t.text(inner, "Rename this file?");
        let ok = t.button(inner, "OK");
        assert_eq!(t.verdict(ok), None);

        // …and the outer dialog's own buttons still see their own question.
        let outer_ok = t.button(outer, "OK");
        assert!(t.verdict(outer_ok).is_some());
    }

    #[test]
    fn a_nested_question_is_read_when_it_is_the_destructive_one() {
        // The other direction of the same boundary: a harmless outer dialog
        // must not shield a destructive inner sheet.
        let mut t = Tree::default();
        let outer = t.dialog_window("Export");
        t.text(outer, "Choose a format.");
        let inner = t.sheet(outer, None);
        t.text(inner, "Overwrite the existing file?");
        let ok = t.button(inner, "OK");
        assert_eq!(t.verdict(ok).as_deref(), Some("overwrite"));
        assert_eq!(
            t.candidate(ok).context.unwrap().question,
            "Overwrite the existing file?".to_string()
        );
    }

    #[test]
    fn a_nested_question_does_not_leak_upward_either() {
        // The outer dialog's own buttons must not inherit the inner sheet's
        // text: the walk goes up from the target, never down into a sibling
        // question.
        let mut t = Tree::default();
        let outer = t.dialog_window("Export");
        t.text(outer, "Choose a format.");
        let inner = t.sheet(outer, None);
        t.text(inner, "Delete the original?");
        let outer_ok = t.button(outer, "OK");
        assert_eq!(t.verdict(outer_ok), None);
    }

    #[test]
    fn the_question_survives_the_layout_between_it_and_the_button() {
        // Real alerts bury their text a few groups down, and a cross-platform
        // toolkit buries it further. Depth inside the context is not the rule;
        // kind of ancestor is.
        let mut t = Tree::default();
        let window = t.window("Project");
        let sheet = t.sheet(window, None);
        let mut parent = sheet;
        for _ in 0..6 {
            parent = t.group(parent);
        }
        t.text(parent, "This will permanently erase 12 recordings.");
        let ok = t.button(sheet, "Continue");
        assert_eq!(t.verdict(ok).as_deref(), Some("eras"));
    }

    #[test]
    fn an_unlabeled_control_in_a_destructive_dialog_fails_closed() {
        // No label means no dismissal exemption. An icon button in a delete
        // sheet is judged by the sheet, which is the fail-closed reading.
        let mut t = Tree::default();
        let window = t.window("Photos");
        let sheet = t.sheet(window, None);
        t.text(sheet, "Delete 12 photos?");
        let icon = t.push(ContextNode {
            parent: Some(sheet),
            role: "AXButton",
            ..ContextNode::default()
        });
        assert!(t.verdict(icon).is_some());
    }

    #[test]
    fn a_silent_dialog_produces_no_evidence() {
        // A sheet with no readable prose is not evidence of anything. It must
        // not refuse by virtue of being a sheet.
        let mut t = Tree::default();
        let window = t.window("Documents");
        let sheet = t.sheet(window, None);
        let ok = t.button(sheet, "OK");
        assert!(t.candidate(ok).context.is_none());
        assert_eq!(t.verdict(ok), None);
    }

    #[test]
    fn the_search_terminates_on_a_malformed_tree() {
        // Parent indices come from a walk of a live app. A cycle should cost a
        // bounded loop, not the server.
        let nodes = vec![
            ContextNode {
                parent: Some(1),
                role: "AXGroup",
                ..ContextNode::default()
            },
            ContextNode {
                parent: Some(0),
                role: "AXGroup",
                ..ContextNode::default()
            },
        ];
        assert!(decision_context(&nodes, 0).is_none());
        assert!(decision_context(&[], 0).is_none());
        // A parent pointing past the end of the tree is not a panic either.
        let dangling = vec![ContextNode {
            parent: Some(99),
            role: "AXButton",
            ..ContextNode::default()
        }];
        assert!(decision_context(&dangling, 0).is_none());
    }

    #[test]
    fn one_question_cannot_cost_an_unbounded_read() {
        // The shape rules decide what is read; this only bounds how much of a
        // pathological dialog is read at all.
        let mut t = Tree::default();
        let window = t.window("Stress");
        let sheet = t.sheet(window, None);
        for _ in 0..200 {
            t.text(sheet, "lorem ipsum");
        }
        let ok = t.button(sheet, "OK");
        let question = t.candidate(ok).context.unwrap().question;
        assert_eq!(
            question.split("lorem ipsum").count() - 1,
            MAX_QUESTION_PARTS
        );
    }

    #[test]
    fn a_context_refusal_quotes_the_question_and_the_way_out() {
        let text = Refused::NeedsConfirmationInContext {
            verb: "click",
            target: "[7] AXButton \"OK\"".to_string(),
            context: "AXSheet".to_string(),
            question: "Delete 4 items?".to_string(),
            matched: "delet".to_string(),
        }
        .to_string();
        assert!(text.contains("Delete 4 items?"));
        assert!(text.contains("AXSheet"));
        assert!(text.contains("confirm_destructive: true"));
        // The distinction the separate variant exists for: the button is not
        // being accused of saying anything destructive.
        assert!(!text.contains("reads as a destructive control"));
    }

    // ── destructive keys ─────────────────────────────────────────────────

    #[test]
    fn command_delete_is_destructive_wherever_it_lands() {
        assert!(destructive_key("cmd+delete", Some(&field("hello"))).is_some());
        assert!(destructive_key("command+backspace", None).is_some());
    }

    #[test]
    fn a_bare_delete_is_editing_in_a_text_field_and_destruction_anywhere_else() {
        assert!(destructive_key("delete", Some(&field("hello"))).is_none());
        assert!(destructive_key("backspace", Some(&field("hello"))).is_none());

        let row = Candidate {
            role: "AXRow".to_string(),
            label: Some("Mom".to_string()),
            description: "[9] AXRow \"Mom\"".to_string(),
            ..Candidate::default()
        };
        assert!(destructive_key("delete", Some(&row)).is_some());
    }

    #[test]
    fn an_unidentifiable_target_makes_a_bare_delete_destructive() {
        // Fail closed: not knowing what the key lands on is not a reason to
        // assume it lands somewhere harmless.
        assert!(destructive_key("delete", None).is_some());
    }

    #[test]
    fn ordinary_keys_and_chords_are_not_destructive() {
        for key in ["return", "escape", "cmd+s", "cmd+shift+p", "a", "tab", "up"] {
            assert!(
                destructive_key(key, Some(&field("hello"))).is_none(),
                "{key:?} must not need confirmation"
            );
        }
    }

    #[test]
    fn key_classification_ignores_case_and_spacing() {
        assert!(destructive_key("  CMD + Delete ", None).is_some());
    }

    // ── flags ────────────────────────────────────────────────────────────

    #[test]
    fn only_one_and_true_switch_a_flag_on() {
        assert!(truthy("1"));
        assert!(truthy("true"));
        assert!(truthy("TRUE"));
        assert!(truthy("True"));
        for off in ["0", "false", "", "yes", "on", "2"] {
            assert!(!truthy(off), "{off:?} must not enable a gate");
        }
    }

    // ── the gate as a whole ──────────────────────────────────────────────

    fn app(name: &str, bundle: &str) -> AppInfo {
        AppInfo {
            name: name.to_string(),
            bundle_id: Some(bundle.to_string()),
            pid: 4242,
            active: false,
            regular: true,
        }
    }

    fn a_gate() -> Gate {
        Gate::at(
            "click",
            &Target::Index {
                index: 1,
                snapshot_id: None,
                expected_role: None,
            },
        )
    }

    #[test]
    fn confirming_clears_the_destructive_gate_and_nothing_else() {
        // Built without touching the environment or the window server: this
        // asserts on the classification the gate consults, which is the part a
        // permission-free test can see. The wiring itself is exercised by the
        // MCP surface tests.
        let confirmed = a_gate().confirmed(true);
        assert!(confirmed.confirm_destructive);
        assert!(!a_gate().confirm_destructive);
    }

    #[test]
    fn a_gate_carries_the_key_for_press_key_and_the_target_for_everything_else() {
        let g = a_gate().with_key("cmd+delete");
        assert_eq!(g.key.as_deref(), Some("cmd+delete"));
        assert!(g.target().is_some());
        assert!(Gate::elementless("click_in_window").target().is_none());
    }

    #[test]
    fn a_forbidden_target_error_names_the_app_and_the_way_out() {
        let text = Refused::ForbiddenTarget {
            verb: "click",
            app: "1Password".to_string(),
            bundle_id: "com.1password.1password".to_string(),
            why: CREDENTIALS,
        }
        .to_string();
        assert!(text.contains("1Password"));
        assert!(text.contains("com.1password.1password"));
        assert!(text.contains("CUA_ALLOW_FORBIDDEN_TARGETS=1"));
    }

    #[test]
    fn a_confirmation_error_names_the_parameter_that_clears_it() {
        let text = Refused::NeedsConfirmation {
            verb: "click",
            target: "[7] AXButton \"Delete All\"".to_string(),
            matched: "delet".to_string(),
        }
        .to_string();
        assert!(text.contains("confirm_destructive: true"));
        assert!(text.contains("Delete All"));
    }

    #[test]
    fn a_key_refusal_blames_the_key_and_not_the_control() {
        let text = Refused::NeedsConfirmationForKey {
            key: "cmd+delete".to_string(),
            target: "[9] AXRow \"Mom\"".to_string(),
            matched: "cmd+delete".to_string(),
        }
        .to_string();
        assert!(text.contains("cmd+delete"));
        assert!(
            !text.contains("destructive control"),
            "a scroll bar is not a destructive control just because Delete was pressed on it"
        );
        assert!(text.contains("confirm_destructive: true"));
    }

    #[test]
    fn a_yield_error_says_how_to_resume() {
        let text = Refused::HumanTookOver {
            verb: "click",
            app: "KakaoTalk".to_string(),
            ago_ms: 120,
            idle_ms: 3_000,
        }
        .to_string();
        assert!(text.contains("3000ms"));
        assert!(text.contains("CUA_YIELD_TO_HUMAN"));
    }

    #[test]
    fn a_watch_that_was_never_started_never_refuses() {
        let watch = HumanWatch::default();
        assert!(watch.since_human_input_ms().is_none());
        assert!(matches!(watch.watch, Watch::Off));
    }

    #[test]
    fn the_screenshot_refusal_explains_itself_when_it_fires() {
        // Calls the classifier directly rather than `screenshot_refusal`, which
        // consults a process-wide env flag another test could have set.
        let a = app("1Password", "com.1password.1password");
        assert!(forbidden_bundle(a.bundle_id.as_deref().unwrap()).is_some());
        let a = app("TextEdit", "com.apple.TextEdit");
        assert!(forbidden_bundle(a.bundle_id.as_deref().unwrap()).is_none());
    }

    #[test]
    fn the_idle_window_is_clamped_into_something_usable() {
        // The parse-and-clamp rule, exercised without mutating the process
        // environment (`yield_idle_ms` caches its answer for the process).
        let parse = |v: &str| {
            v.trim()
                .parse::<u64>()
                .ok()
                .map(|n| n.clamp(250, 60_000))
                .unwrap_or(3_000)
        };
        assert_eq!(parse("5000"), 5_000);
        assert_eq!(parse("1"), 250);
        assert_eq!(parse("999999"), 60_000);
        assert_eq!(parse("nonsense"), 3_000);
    }

    // ── the session scope ────────────────────────────────────────────────────

    #[test]
    fn an_allowlist_is_split_trimmed_and_lowercased() {
        assert_eq!(
            parse_allowlist("com.kakao.KakaoTalkMac , com.apple.TextEdit"),
            vec![
                "com.kakao.kakaotalkmac".to_string(),
                "com.apple.textedit".to_string()
            ]
        );
        assert_eq!(
            parse_allowlist("  com.apple.Safari  "),
            vec!["com.apple.safari".to_string()]
        );
    }

    #[test]
    fn a_scope_that_names_nothing_admits_nothing() {
        // `CUA_ALLOWED_APPS=$TYPO` expands to an empty value, and a gate that
        // opens itself on a misspelling fails in the wrong direction. So an
        // empty scope is an empty scope: everything is refused, loudly and
        // immediately. Unsetting the variable is how to ask for unscoped.
        for raw in ["", "   ", ",,", " , , "] {
            let list = parse_allowlist(raw);
            assert!(list.is_empty(), "{raw:?} should name no app");
            assert!(
                !in_scope(&list, "com.apple.TextEdit"),
                "{raw:?} must not admit anything"
            );
        }
    }

    #[test]
    fn scope_matching_ignores_case_and_surrounding_space() {
        let list = parse_allowlist("com.kakao.KakaoTalkMac");
        assert!(in_scope(&list, "com.kakao.KakaoTalkMac"));
        assert!(in_scope(&list, "com.kakao.kakaotalkmac"));
        assert!(in_scope(&list, "  com.kakao.KakaoTalkMac  "));
        assert!(!in_scope(&list, "com.apple.TextEdit"));
    }

    #[test]
    fn a_scope_entry_never_matches_a_prefix() {
        // The failure this rules out is a scope of `com.apple.Safari` silently
        // admitting Safari Technology Preview, and a scope of `com.apple`
        // admitting every Apple app on the machine.
        let one = parse_allowlist("com.apple.Safari");
        assert!(!in_scope(&one, "com.apple.SafariTechnologyPreview"));

        let vendor = parse_allowlist("com.apple");
        assert!(!in_scope(&vendor, "com.apple.Safari"));
        assert!(!in_scope(&vendor, "com.apple.TextEdit"));
    }

    #[test]
    fn the_scope_and_the_forbidden_floor_are_independent() {
        // Scoping a run to a password manager must not lift the floor: the
        // allowlist widens nothing, it only narrows. `guard` checks the floor
        // first for exactly this reason.
        let list = parse_allowlist("com.1password.1password");
        assert!(in_scope(&list, "com.1password.1password"));
        assert!(forbidden_bundle("com.1password.1password").is_some());
    }

    // ── Return presses the default button, not the aimed one ─────────────────

    #[test]
    fn only_an_unmodified_return_presses_the_default_button() {
        for key in [
            "return",
            "enter",
            "Return",
            " ENTER ",
            "kp_enter",
            "numpad_enter",
        ] {
            assert!(
                key_activates_default_button(key),
                "{key:?} should be treated as pressing the default button"
            );
        }
        // Escape activates the *cancel* button, which is safe by construction,
        // and space presses the focused control, which is the aimed-element case
        // the rest of the gate already judges correctly.
        for key in ["escape", "space", "a", "delete", "tab", "down"] {
            assert!(!key_activates_default_button(key), "{key:?}");
        }
        // A modified Return is an app shortcut, not "confirm this dialog".
        for key in ["cmd+return", "shift+enter", "ctrl+alt+return"] {
            assert!(!key_activates_default_button(key), "{key:?}");
        }
    }

    #[test]
    fn substituting_the_answer_keeps_the_question() {
        let (sheet, answers) =
            confirm_sheet("Delete 4 items?", "This cannot be undone.", &["Cancel"]);
        let cancel = answers[0];

        // Aimed at Cancel, the gate is right to allow it: cancelling is how a
        // caller avoids the destruction.
        let mut c = sheet.candidate(cancel);
        assert!(destructive_context(&c).is_none(), "Cancel must stay exempt");

        // But `return` will press Delete. Substituting the answer must flip the
        // verdict while the question stays the same one.
        let before = c.context.clone();
        c.substitute_answer(
            "AXButton",
            Some("Delete".to_string()),
            "[9] AXButton \"Delete\"".into(),
        );
        assert_eq!(c.context, before, "the question is unchanged");
        assert!(
            destructive_token(&c.classifiable_text()).is_some(),
            "Delete is destructive on its own label"
        );
    }

    #[test]
    fn a_substituted_terse_default_is_caught_by_the_question() {
        // The harder shape: the default button says nothing, so only the
        // question can convict it. This is the case the whole context rule
        // exists for, reached through a key rather than a click.
        let (sheet, answers) =
            confirm_sheet("4개 항목을 삭제할까요?", "되돌릴 수 없습니다.", &["취소"]);
        let cancel = answers[0];

        let mut c = sheet.candidate(cancel);
        assert!(destructive_context(&c).is_none(), "취소 must stay exempt");

        c.substitute_answer(
            "AXButton",
            Some("확인".to_string()),
            "[9] AXButton \"확인\"".into(),
        );
        let (_, matched) = destructive_context(&c).expect("확인 under a 삭제 question is refused");
        assert!(matched.contains("삭제"), "matched {matched:?}");
    }

    #[test]
    fn substitution_does_not_carry_the_aimed_elements_exemptions() {
        // Aimed at a text field, which is exempt from context evidence because
        // typing into a sheet is not the decision. If substitution kept
        // `settable`, the field would excuse the button Return actually presses.
        let mut sheet = Tree::default();
        let window = sheet.window("Documents");
        let ctx = sheet.sheet(window, None);
        let body = sheet.group(ctx);
        sheet.text(body, "Delete 4 items?");
        let name = sheet.field(ctx, "notes about deleting things");

        let mut c = sheet.candidate(name);
        assert!(
            destructive_context(&c).is_none(),
            "a field is not the decision"
        );

        c.substitute_answer(
            "AXButton",
            Some("OK".to_string()),
            "[9] AXButton \"OK\"".into(),
        );
        assert!(!c.settable, "the field's writability must not survive");
        assert!(c.value.is_none(), "the field's text must not survive");
        assert!(
            destructive_context(&c).is_some(),
            "OK under a delete question is refused even when a field was aimed at"
        );
    }
}
