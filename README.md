# Epistel

Epistel is a small mail server that lets ordinary mail clients (Outlook,
Thunderbird, Apple Mail, mobile clients) read and send mail for addresses
that live on a cloud mail service. The first supported service is Amazon SES.

SES can receive mail and send mail, but it has no mailboxes and speaks no
protocol a mail client can read from. Epistel fills that gap: it keeps the
mailboxes, serves them over IMAP, and accepts outgoing mail over SMTP
Submission, handing it to SES for delivery.

Status: early development. Mail comes in and can be read: backend traits with
in-memory and AWS implementations (DynamoDB, S3, SES over SQS), conformance
suites, administration commands, ingest, and an IMAP server over TLS. IMAP is
read-only so far; changing mail over IMAP and sending through Submission are
next. See:

- [doc/architecture.md](doc/architecture.md) - the design
- [doc/roadmap.md](doc/roadmap.md) - what the first release contains, and
  what is deliberately left for later

The matching infrastructure code lives in the `terraform-aws-epistel`
repository.

## What it is

- IMAP on port 993 and SMTP Submission on port 465, both implicit TLS.
  Nothing else: no STARTTLS, no POP3, no plaintext, no port 25.
- Not a mail transfer agent. Receiving from and delivering to the internet,
  DKIM signing, and spam and virus scanning stay with the mail service.
- Stateless process. Messages live in an object store, everything else in a
  metadata store. Both sit behind traits and are chosen by configuration.

## Building

```
cargo test --workspace --all-features
cargo run -p epistel -- --config epistel.example.toml check
```

The AWS backends are tested against emulators. Their tests run when
`EPISTEL_TEST_DYNAMODB_ENDPOINT`, `EPISTEL_TEST_S3_ENDPOINT` and
`EPISTEL_TEST_SQS_ENDPOINT` name one, and report themselves skipped otherwise;
see `crates/epistel-aws/tests/emulators.rs`.

## Running

```
epistel serve
```

files incoming mail and serves IMAP on port 993 until stopped. It needs a
certificate and key for the host name (`[tls]` in the configuration) and takes
over new ones when the files change. It logs lines on a terminal and JSON
otherwise.

## Administration

```
epistel account add ada@example.org --name "Ada"
epistel password new ada@example.org --label laptop
epistel address add sales@example.org ada@example.org
epistel address add '*@example.org' ada@example.org
```

`epistel help` lists the rest.

## License

Apache-2.0. See [LICENSE](LICENSE).
