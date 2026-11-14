# Epistel roadmap

## Release 0.1

The smallest thing that lets a mail client use a domain hosted on Amazon
SES. Design in [architecture.md](architecture.md).

Contents:

- IMAP on 993 and SMTP Submission on 465, implicit TLS, SASL PLAIN with app
  passwords.
- Metadata in DynamoDB, messages in S3, inbound from SES receipt events over
  SQS, outbound through the SES v2 send API. All four behind traits, with
  in-memory implementations and conformance suites.
- Accounts, app passwords, addresses, aliases, catch-all, managed from the
  command line.
- Personal mailboxes only, with the standard special-use folders.
- Junk filing and virus quarantine from the mail service's verdicts.
- One instance, TLS certificate from files.

Milestones, in order. All seven are done; 0.1.0 is their release.

1. **Foundation.** Workspace, core types, the five traits, in-memory
   implementations, conformance suites, configuration loading, continuous
   integration.
2. **AWS storage.** DynamoDB and S3 implementations passing the suites.
   Admin commands.
3. **Ingest.** SES over SQS source, address resolution, filing, exactly-once
   delivery, dead-letter handling, the `serve` command.
4. **IMAP, read path.** Message structure and address parsing, TLS
   listener with certificate reload, login, `LIST`, `STATUS`, `SELECT`,
   `FETCH`, `SEARCH`. Mailboxes open read-only.
5. **IMAP, write path.** `STORE`, `APPEND`, `COPY`, `MOVE`, `EXPUNGE`,
   mailbox management, `IDLE` with change notification, updates between
   sessions.
6. **Submission.** Listener, `AUTH PLAIN` and `LOGIN`, sender authorization
   for envelope and `From`, message completion, relay through the SES send
   API.
7. **Packaging.** Mailboxes declared in the configuration, the container
   image for x86-64, published with each release.

## Next: hardening

Planned for the release after 0.1.0, in no fixed order:

- Header block kept on the message record, so that listing a mailbox does
  not read blobs.
- Rate limits for failed logins per source address and per account, with
  exponential delay, and connection limits per source and per account.
  0.1.0 answers every failed login after a fixed delay and caps each
  listener at 2048 connections.
- Graceful shutdown: `BYE` to IMAP sessions, Submission transactions in
  flight finished.
- Fuzzing of the IMAP and SMTP parsers.
- The manual client matrix.
- The sweep for unreferenced blobs and items, and batched expunge on
  DynamoDB.
- Release artifacts: static binaries for x86-64 and ARM64 Linux, and a
  container image for ARM64.

The infrastructure module is developed alongside, from milestone 2 on, in
the `terraform-aws-epistel` repository.

## Not in 0.1

Everything here is deliberately excluded from the first release. Nothing in
the 0.1 design blocks it.

### Mail features

| Item | Note |
|---|---|
| Shared mailboxes | Mailboxes owned by a group, with IMAP `ACL` and a shared namespace. |
| Server-side filters | Sieve scripts and ManageSieve. Until then, filtering is done by the client. |
| Quotas | Per-account storage limits and the IMAP `QUOTA` extension. |
| `CONDSTORE` and `QRESYNC` | Fast resynchronization. The modification sequence is already stored. |
| Full-text search | An index for `BODY` and `TEXT` searches. |
| `SORT` and `THREAD` | Server-side ordering and threading. |
| Delivery status processing | Reading bounce and complaint events from the mail service instead of relying on bounce mail. |
| Vacation replies | Depends on server-side filters. |

### Protocols and authentication

| Item | Note |
|---|---|
| OAUTHBEARER | Useful once clients offer OAuth for arbitrary servers. |
| IMAP4rev2 | Announcing the newer revision: extended `LIST`, `ESEARCH` results, `STATUS` sizes. Every client in use speaks IMAP4rev1. |
| Renaming the inbox | IMAP gives it a special meaning: the messages move, the inbox stays. Refused for now. |
| Decoding mailbox names | Names are stored as sent; decoding modified UTF-7 would let them show properly outside IMAP. |
| JMAP | Candidate for a second access protocol. |
| Client autoconfiguration | Thunderbird autoconfig document and DNS service records are static and can be published by the infrastructure; a built-in endpoint is optional. |
| Built-in ACME | The server obtaining its own certificate, cached in the blob store. |

Never planned: POP3, STARTTLS, plaintext listeners, port 25.

### Backends

The traits exist so that these are additions, not rewrites.

| Trait | Candidates |
|---|---|
| `MetadataStore` | PostgreSQL, SQLite |
| `BlobStore` | any S3-compatible store, local file system, Azure Blob Storage, Google Cloud Storage |
| `InboundSource` | LMTP or SMTP listener for delivery from a local mail transfer agent; other cloud mail services |
| `OutboundRelay` | generic authenticated SMTP relay; other cloud mail services |
| `ChangeBus` | a shared bus for more than one instance |

### Operations

| Item | Note |
|---|---|
| More than one instance | Needs the shared `ChangeBus` and shared rate-limit state. |
| Metrics | Prometheus endpoint or OpenTelemetry export. |
| Other deployment shapes | Container service behind a load balancer, Kubernetes. The server already runs anywhere it can bind two ports and read a certificate. |
| Administration interface | HTTP API and a web console on top of it. |
| Import and export | Bringing mail in from another IMAP server; exporting an account. |
| Schema migrations | Start with 1.0. Before that, releases may require an empty store. |
