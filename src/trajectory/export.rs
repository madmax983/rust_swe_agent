use crate::trajectory::Trajectory;

pub fn to_bash_script(trajectory: &Trajectory) -> String {
    let mut script = String::new();

    for msg in &trajectory.messages {
        if msg.role == "assistant" {
            if let Some(actions) = &msg.extra.actions {
                if !actions.is_empty() {
                    // Prepend the assistant's prose content as a comment block
                    let mut commented_content = String::new();
                    for line in msg.content.lines() {
                        commented_content.push_str("# ");
                        commented_content.push_str(line);
                        commented_content.push('\n');
                    }
                    if !commented_content.is_empty() {
                        script.push_str(&commented_content);
                    }

                    // Add the bash actions
                    for action in actions {
                        script.push_str(action);
                    }
                    script.push('\n');
                }
            }
        }
    }

    script
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Message;

    #[test]
    fn test_to_bash_script() {
        let mut t = Trajectory::new();

        let mut msg1 = Message::assistant("I will list the directory.");
        msg1.extra.actions = Some(vec!["ls -la\n".to_string()]);
        t.record_with_extra(&msg1, msg1.extra.clone());

        let mut msg2 = Message::assistant("I will print hello world.");
        msg2.extra.actions = Some(vec!["echo 'Hello, World!'\n".to_string()]);
        t.record_with_extra(&msg2, msg2.extra.clone());

        let script = to_bash_script(&t);

        let expected = "# I will list the directory.\nls -la\n\n# I will print hello world.\necho 'Hello, World!'\n\n";
        assert_eq!(script, expected);
    }

    #[test]
    fn test_to_bash_script_empty_and_no_actions() {
        let mut t = Trajectory::new();

        let msg1 = Message::assistant("I am thinking...");
        t.record_with_extra(&msg1, msg1.extra.clone());

        let mut msg2 = Message::assistant("");
        msg2.extra.actions = Some(vec![]);
        t.record_with_extra(&msg2, msg2.extra.clone());

        let msg3 = Message::user("User input");
        t.record_with_extra(&msg3, msg3.extra.clone());

        let script = to_bash_script(&t);

        let expected = "";
        assert_eq!(script, expected);
    }

    #[test]
    fn test_to_bash_script_no_content_with_action() {
        let mut t = Trajectory::new();

        let mut msg1 = Message::assistant("");
        msg1.extra.actions = Some(vec!["echo hi\n".to_string()]);
        t.record_with_extra(&msg1, msg1.extra.clone());

        let script = to_bash_script(&t);

        let expected = "echo hi\n\n";
        assert_eq!(script, expected);
    }
}
