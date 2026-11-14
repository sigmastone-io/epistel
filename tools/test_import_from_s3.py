# SPDX-FileCopyrightText: 2026 Sigmastone OÜ
# SPDX-License-Identifier: Apache-2.0

"""Tests for the parts of import_from_s3.py that decide where a message goes.

    python -m unittest discover -s tools -p "test_*.py"
"""

import email.parser
import unittest

import import_from_s3 as tool

MAILBOXES = {"legal@example.org", "support@example.org", "legal@example.net"}


def headers(text):
    return email.parser.BytesParser().parsebytes(text.replace("\n", "\r\n").encode(), headersonly=True)


class Passwords(unittest.TestCase):
    def test_saved_command_output(self):
        text = (
            "app password for Legal@Example.org (id 0123456789abcdef0123456789abcdef)\n\n"
            "    abcdefghijklmnopqrstuvwxyz234567\n\n"
            "It is shown this once and cannot be recovered. Enter it in the mail client as the\n"
            "password for both incoming and outgoing mail; the user name is legal@example.org.\n"
            "app password for support@example.org (id fedcba9876543210fedcba9876543210)\n\n"
            "    zyxwvutsrqponmlkjihgfedcba765432\n"
        )
        self.assertEqual(
            tool.read_passwords(text),
            {"legal@example.org": "abcdefghijklmnopqrstuvwxyz234567", "support@example.org": "zyxwvutsrqponmlkjihgfedcba765432"},
        )

    def test_plain_lines_and_the_first_password_wins(self):
        text = "legal@example.org abcdefghijklmnopqrstuvwxyz234567\n== support@example.org\n    zyxwvutsrqponmlkjihgfedcba765432\n    aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n"
        found = tool.read_passwords(text)
        self.assertEqual(found["legal@example.org"], "abcdefghijklmnopqrstuvwxyz234567")
        self.assertEqual(found["support@example.org"], "zyxwvutsrqponmlkjihgfedcba765432")

    def test_an_id_is_not_taken_for_a_password(self):
        self.assertEqual(tool.read_passwords("legal@example.org (id 0123456789abcdef0123456789abcdef)\n"), {})


class Recipients(unittest.TestCase):
    def test_the_address_ses_recorded_wins_over_the_headers(self):
        message = headers(
            "Received: from mta.elsewhere.example (mta.elsewhere.example [192.0.2.1])\n"
            " by inbound-smtp.eu-west-1.amazonaws.com with SMTP id abc\n"
            " for Legal@Example.org;\n Mon, 05 Jan 2026 10:00:00 +0000 (UTC)\n"
            "To: support@example.org\nSubject: x\n\n"
        )
        self.assertEqual(tool.recipients_of(message, MAILBOXES), ["legal@example.org"])

    def test_to_and_cc_are_used_when_nothing_was_recorded(self):
        message = headers(
            "Received: from a by b; Mon, 05 Jan 2026 10:00:00 +0000\n"
            'To: "Support, Team" <Support@example.org>, someone@elsewhere.example\n'
            "Cc: legal@example.net\n\n"
        )
        self.assertEqual(tool.recipients_of(message, MAILBOXES), ["legal@example.net", "support@example.org"])

    def test_a_message_for_someone_else_names_no_mailbox(self):
        message = headers("Received: from a by b for <other@example.org>;\nTo: other@example.org\n\n")
        self.assertEqual(tool.recipients_of(message, MAILBOXES), [])


class Filing(unittest.TestCase):
    def test_verdicts_decide_the_folder(self):
        self.assertEqual(tool.folder_for(headers("X-SES-Spam-Verdict: PASS\nX-SES-Virus-Verdict: PASS\n\n")), "INBOX")
        self.assertEqual(tool.folder_for(headers("X-SES-Spam-Verdict: FAIL\nX-SES-Virus-Verdict: PASS\n\n")), "Junk")
        self.assertIsNone(tool.folder_for(headers("X-SES-Spam-Verdict: FAIL\nX-SES-Virus-Verdict: FAIL\n\n")))
        self.assertEqual(tool.folder_for(headers("Subject: no verdicts\n\n")), "INBOX")

    def test_message_ids_compare_without_brackets_case_or_folding(self):
        self.assertEqual(tool.message_id(headers("Message-ID:\n <ABC@Example.org>\n\n")), "abc@example.org")
        self.assertEqual(tool.message_id(headers("Subject: none\n\n")), "")


if __name__ == "__main__":
    unittest.main()
