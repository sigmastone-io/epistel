#!/usr/bin/env python3

# SPDX-FileCopyrightText: 2026 Sigmastone OÜ
# SPDX-License-Identifier: Apache-2.0

"""Bring mail that SES stored in an S3 bucket into Epistel mailboxes.

For a domain that received mail through an SES receipt rule with an S3 action before Epistel
was set up: the raw messages are still in that bucket, for as long as its lifecycle keeps
them. This reads them and hands each one to the server over IMAP, into the mailbox of the
address it was sent to, with the date it arrived.

It only reads the bucket. It writes through the server's own front door, the same way a mail
client does when a message is dragged into a folder, so it needs nothing but the Python
standard library, the AWS CLI, and the app passwords of the mailboxes.

    # Look first: nothing is written.
    python import_from_s3.py --bucket my-mail-bucket --prefix inbound/ \\
        --host mail.example.org --passwords passwords.txt

    # Then do it.
    python import_from_s3.py --bucket my-mail-bucket --prefix inbound/ \\
        --host mail.example.org --passwords passwords.txt --apply

Which mailbox a message goes to is read from the message: the address SES recorded in its
`Received: ... for <address>` line, otherwise the mailboxes named in `To` and `Cc`. A message
that names none of the mailboxes (it arrived by `Bcc`) is reported and left out, unless
`--fallback` names a mailbox to collect such mail.

Running it again is safe. A message is skipped when the mailbox already holds one with the
same `Message-ID`, which also covers mail the server received itself in the meantime, and
when the state file says this run's predecessor imported it.

The passwords file is the output of `epistel password new`, saved as it was printed, or lines
of `address password`. It is read, never printed.
"""

import argparse
import email.parser
import email.utils
import imaplib
import json
import re
import subprocess
import sys
import tempfile
from collections import Counter
from datetime import datetime, timezone
from pathlib import Path

RECEIVED_FOR = re.compile(r"\bfor\s+<?([^\s<>;]+@[^\s<>;]+)>?", re.IGNORECASE)
APP_PASSWORD = re.compile(r"\b[a-z2-7]{32}\b")
ADDRESS = re.compile(r"[^\s<>()\"',;:]+@[^\s<>()\"',;:]+")
# What SES leaves in the bucket when a rule is first pointed at it. Not mail.
SETUP_NOTIFICATION = "AMAZON_SES_SETUP_NOTIFICATION"


def read_passwords(text):
    """Address to app password, from saved `epistel password new` output or `address password` lines."""
    passwords = {}
    current = None
    for line in text.splitlines():
        addresses = ADDRESS.findall(line)
        secrets = APP_PASSWORD.findall(line)
        if addresses:
            current = addresses[0].lower().rstrip(".")
        if secrets and current:
            # The first password after an address is that address's; later ones belong to other
            # devices and work just as well, but one is enough.
            passwords.setdefault(current, secrets[0])
    return passwords


def recipients_of(headers, mailboxes):
    """The mailboxes a message was sent to, from most to least reliable evidence."""
    recorded = set()
    for received in headers.get_all("Received", []):
        for address in RECEIVED_FOR.findall(" ".join(str(received).split())):
            recorded.add(address.lower())
    hits = sorted(recorded & mailboxes)
    if hits:
        return hits
    named = set()
    for name in ("To", "Cc"):
        for _, address in email.utils.getaddresses([str(value) for value in headers.get_all(name, [])]):
            named.add(address.lower())
    return sorted(named & mailboxes)


def verdict(headers, name):
    return str(headers.get(name, "")).strip().upper()


def folder_for(headers):
    """Where the server itself would have filed the message; None for mail that failed the virus check."""
    if verdict(headers, "X-SES-Virus-Verdict") == "FAIL":
        return None
    if verdict(headers, "X-SES-Spam-Verdict") == "FAIL":
        return "Junk"
    return "INBOX"


def message_id(headers):
    return " ".join(str(headers.get("Message-ID", "")).split()).strip("<>").lower()


def one_line(value, width):
    text = " ".join(str(value or "").split())
    return text if len(text) <= width else text[: width - 1] + "~"


def aws(arguments, options):
    command = ["aws"] + arguments
    if options.region:
        command += ["--region", options.region]
    if options.profile:
        command += ["--profile", options.profile]
    done = subprocess.run(command, capture_output=True)
    if done.returncode != 0:
        sys.exit("aws {}: {}".format(" ".join(arguments[:2]), done.stderr.decode(errors="replace").strip()))
    return done.stdout


def list_objects(options):
    """Key to arrival time, for every object under the prefix."""
    listed = aws(["s3api", "list-objects-v2", "--bucket", options.bucket, "--prefix", options.prefix, "--output", "json"], options)
    arrived = {}
    for entry in json.loads(listed or b"{}").get("Contents", []):
        if entry["Size"] > 0 and not entry["Key"].endswith(SETUP_NOTIFICATION):
            arrived[entry["Key"]] = datetime.fromisoformat(entry["LastModified"].replace("Z", "+00:00")).astimezone(timezone.utc)
    return arrived


class Server:
    """IMAP connections to the mailboxes, opened when first needed."""

    def __init__(self, host, port, passwords):
        self.host, self.port, self.passwords = host, port, passwords
        self.connections = {}
        self.known = {}

    def connection(self, mailbox):
        if mailbox not in self.connections:
            connection = imaplib.IMAP4_SSL(self.host, self.port)
            connection.login(mailbox, self.passwords[mailbox])
            self.connections[mailbox] = connection
        return self.connections[mailbox]

    def message_ids(self, mailbox, folder):
        """The Message-IDs already in a folder."""
        if (mailbox, folder) not in self.known:
            connection = self.connection(mailbox)
            found = set()
            status, _ = connection.select(folder, readonly=True)
            if status == "OK":
                _, data = connection.fetch("1:*", "(BODY.PEEK[HEADER.FIELDS (MESSAGE-ID)])")
                for part in data or []:
                    if isinstance(part, tuple):
                        found.add(message_id(email.parser.BytesParser().parsebytes(part[1], headersonly=True)))
                connection.close()
            found.discard("")
            self.known[(mailbox, folder)] = found
        return self.known[(mailbox, folder)]

    def append(self, mailbox, folder, flags, arrived, raw):
        status, answer = self.connection(mailbox).append(folder, flags, imaplib.Time2Internaldate(arrived), raw)
        if status != "OK":
            raise RuntimeError("{} refused the message: {}".format(mailbox, answer))

    def close(self):
        for connection in self.connections.values():
            try:
                connection.logout()
            except Exception:
                pass


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0], formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--bucket", required=True, help="the bucket SES stored mail in")
    parser.add_argument("--prefix", default="", help="the prefix the receipt rule wrote under, such as inbound/")
    parser.add_argument("--host", required=True, help="the Epistel server's host name")
    parser.add_argument("--port", type=int, default=993)
    parser.add_argument("--passwords", required=True, type=Path, help="file with the app passwords of the mailboxes")
    parser.add_argument("--fallback", help="mailbox for mail that names none of the mailboxes")
    parser.add_argument("--unread", action="store_true", help="import as unread; the default marks old mail as read")
    parser.add_argument("--state", type=Path, default=Path("epistel-import-state.json"), help="remembers what was imported")
    parser.add_argument("--summary", action="store_true", help="print totals only, no line per message")
    parser.add_argument("--region", help="AWS region of the bucket")
    parser.add_argument("--profile", help="AWS CLI profile")
    parser.add_argument("--apply", action="store_true", help="write to the mailboxes; without it nothing is changed")
    options = parser.parse_args()

    passwords = read_passwords(options.passwords.read_text(encoding="utf-8", errors="replace"))
    if not passwords:
        sys.exit("{} holds no address with an app password".format(options.passwords))
    mailboxes = set(passwords)
    fallback = options.fallback.lower() if options.fallback else None
    if fallback and fallback not in mailboxes:
        sys.exit("--fallback {} has no password in {}".format(fallback, options.passwords))
    done_before = set(json.loads(options.state.read_text())) if options.state.exists() else set()

    arrived = list_objects(options)
    print("{} messages in s3://{}/{}; {} mailboxes".format(len(arrived), options.bucket, options.prefix, len(mailboxes)))
    server = Server(options.host, options.port, passwords)
    counts = Counter()
    per_mailbox = Counter()
    imported = set(done_before)

    with tempfile.TemporaryDirectory() as scratch:
        # One download for everything; the copies are removed when the run ends.
        aws(["s3", "sync", "s3://{}/{}".format(options.bucket, options.prefix), scratch, "--only-show-errors"], options)
        for key in sorted(arrived, key=arrived.get):
            path = Path(scratch) / key[len(options.prefix):]
            if not path.is_file():
                continue
            raw = path.read_bytes()
            headers = email.parser.BytesParser().parsebytes(raw, headersonly=True)
            folder = folder_for(headers)
            targets = recipients_of(headers, mailboxes) or ([fallback] if fallback else [])
            when = arrived[key].strftime("%Y-%m-%d %H:%M")
            described = "{}  {:<28} {}".format(when, one_line(headers.get("From"), 28), one_line(headers.get("Subject"), 50))
            if folder is None:
                counts["failed the virus check, left out"] += 1
                continue
            if not targets:
                counts["names no mailbox, left out"] += 1
                if not options.summary:
                    print("  ?  {}".format(described))
                continue
            for mailbox in targets:
                mark = "{}|{}".format(mailbox, key)
                identifier = message_id(headers)
                if mark in done_before or (identifier and identifier in server.message_ids(mailbox, folder)):
                    counts["already there, skipped"] += 1
                    continue
                if options.apply:
                    server.append(mailbox, folder, "" if options.unread else "(\\Seen)", arrived[key], raw)
                    if identifier:
                        server.message_ids(mailbox, folder).add(identifier)
                    imported.add(mark)
                    options.state.write_text(json.dumps(sorted(imported)))
                counts["imported" if options.apply else "would be imported"] += 1
                per_mailbox["{} {}".format(mailbox, folder)] += 1
                if not options.summary:
                    print("  {}  {:<26} {}".format("+" if options.apply else "-", mailbox + " " + folder, described))
    server.close()

    print()
    for label, count in sorted(counts.items()):
        print("{:>6}  {}".format(count, label))
    for label, count in sorted(per_mailbox.items()):
        print("{:>6}    {}".format(count, label))
    if not options.apply:
        print("\nNothing was written. Add --apply to import.")


if __name__ == "__main__":
    main()
