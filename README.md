# Epistel

Epistel is a small mail server that lets ordinary mail clients (Outlook,
Thunderbird, Apple Mail, mobile clients) read and send mail for addresses
that live on a cloud mail service. The first supported service is Amazon SES.

SES can receive mail and send mail, but it has no mailboxes and speaks no
protocol a mail client can read from. Epistel fills that gap: it keeps the
mailboxes, serves them over IMAP, and accepts outgoing mail over SMTP
Submission, handing it to SES for delivery.

Status: early development, feature complete for the first release. Mail comes
in, can be read and managed, and goes out: backend traits with in-memory and
AWS implementations (DynamoDB, S3, SES over SQS, SES send API), conformance
suites, administration commands, ingest, an IMAP4rev1 server with `IDLE`,
`MOVE` and `UIDPLUS`, and a Submission server, both over TLS only. What is
left before 0.1 is hardening and trial against real mail clients and a real
SES account. See:

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

files incoming mail, serves IMAP on port 993 and takes mail to send on port
465, until stopped. It needs a
certificate and key for the host name (`[tls]` in the configuration) and takes
over new ones when the files change. It logs lines on a terminal and JSON
otherwise.

## Container image

```
docker build -t epistel .
docker run -p 993:993 -p 465:465 -v /etc/epistel:/etc/epistel:ro epistel
```

The image holds the one binary and runs it as an unprivileged user. The
configuration is expected at `/etc/epistel/epistel.toml`. Every push to `dev`
builds the image and publishes it to this repository's registry; it also goes
to an Amazon ECR repository when the repository variables `AWS_ROLE_ARN`,
`AWS_REGION` and `ECR_REPOSITORY` name one.

## Administration

Mailboxes can be declared in the configuration (`[[mailbox]]`); the server
makes the missing ones when it starts. Or by hand:

```
epistel account add ada@example.org --name "Ada"
epistel password new ada@example.org --label laptop
epistel address add sales@example.org ada@example.org
epistel address add '*@example.org' ada@example.org
```

`epistel help` lists the rest.

## Bringing in older mail

A domain that received mail through an SES receipt rule with an S3 action
before Epistel was set up still has those messages in that bucket, for as long
as the bucket keeps them. `tools/import_from_s3.py` reads them and hands each
to the server over IMAP, into the mailbox of the address it was sent to, with
the date it arrived. It needs Python, the AWS CLI and the mailboxes' app
passwords, changes nothing in the bucket, and can be run again safely. Without
`--apply` it only reports what it would do.

```
python tools/import_from_s3.py --bucket my-mail-bucket --prefix inbound/     --host mail.example.org --passwords passwords.txt
```

## License

Apache-2.0. See [LICENSE](LICENSE).
