# ADR 0003: Plain text by default, no remote content in HTML view

## Status

Accepted

## Date

2026-04-03

## Context

HTML emails are the primary vector for email tracking and privacy violations. Tracking pixels (invisible 1x1 images), remote fonts, and external CSS allow senders to know when, where, and on what device a message was opened. HTML emails also present a larger attack surface for phishing — styled content can disguise malicious links more effectively than plain text.

Most email clients default to rendering full HTML with remote content, sometimes offering a "block remote images" toggle. This still exposes users to styled phishing content and any tracking embedded in the HTML structure itself.

## Decision

The message reader defaults to **plain text view**. When a message contains both text and HTML parts (multipart/alternative), the text part is shown. A toggle button (`[Plain Text | HTML]`) allows the user to switch to the sanitized HTML view when needed.

When viewing HTML:
- All `<img>` tags are stripped server-side by the `ammonia` sanitizer before the HTML reaches the frontend. This eliminates tracking pixels and remote image loading at the source.
- No external resources (fonts, stylesheets, iframes) are loaded.
- A "Remote content blocked" notice is displayed.
- Links are not navigable — clicking copies the URL to clipboard (see ADR 0001).

If a message has only an HTML part with no text alternative, the sanitized HTML is shown as a fallback since there is no text to display.

Switching between messages resets the view back to plain text.

## Consequences

- Privacy by default: no tracking pixels fire, no remote requests leave the client.
- Security by default: plain text eliminates styled phishing and HTML-based exploits.
- Users who need to see formatted content (tables, styled newsletters) can opt in per-message with one click.
- Some messages may look sparse in plain text if the sender provided a minimal text part. The HTML toggle is always one click away.
- Image-heavy emails (product announcements, marketing) will appear without images even in HTML mode. A future "load remote content for this message" feature could address this with explicit user consent.
