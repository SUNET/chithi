# Privacy policy

Last updated: 2026-08-28.

Chithi is a desktop email, calendar, contacts, and video-conferencing
client developed in the open at
[github.com/SUNET/chithi](https://github.com/SUNET/chithi). This page
describes how Chithi handles your personal data and how you can
exercise your rights under applicable data-protection law, in
particular the EU General Data Protection Regulation (GDPR) and the
UK GDPR.

## What Chithi the application processes

Chithi runs entirely on your own device. When you connect a mail,
calendar, contacts, or video-conferencing account, Chithi:

- On desktop, stores account passwords and OAuth tokens in the
  operating-system keyring. Android OAuth tokens are stored in the
  application's private sandbox.
- Stores cached copies of mail, calendar entries, and contacts in a
  local database under your user profile.
- Sends and receives account data directly between your device and the
  providers you configure (your IMAP/SMTP/CalDAV/CardDAV server, Gmail,
  Microsoft 365, Nextcloud Talk, Matrix, Zoom, La Suite Visio, and similar).

Supporting operations can contact other services: OAuth authorization
and token endpoints, DNS and account auto-configuration sources, the
static `chithi.org` Zoom redirect, WKD servers when you request an
OpenPGP key, and remote-image hosts when you choose to load images.

The Chithi authors do not operate any backend that receives your mail,
calendar, contact, or meeting data. We have no copy of, and no access to,
the content stored on your device or transmitted between your device and
your providers.

For information on how those providers handle your data once it
leaves Chithi, please consult their respective privacy policies
(Google, Microsoft, Zoom, your mail or calendar host, and any
others). They are independent data controllers for the data you
exchange with them.

## What chithi.org collects

The chithi.org website is a static documentation site hosted on
GitHub Pages. It does not run server-side code under our control,
does not set tracking cookies, and does not include analytics
scripts. GitHub may log standard request metadata such as IP address
and user agent for the underlying GitHub Pages service: see
[GitHub's privacy statement](https://docs.github.com/site-policy/privacy-policies/github-general-privacy-statement)
for what GitHub does with that data.

The page at [chithi.org/oauth/zoom](https://chithi.org/oauth/zoom) is
a static OAuth redirect helper. GitHub Pages infrastructure receives
the request URL, which contains a short-lived Zoom authorization code.
Client-side JavaScript then forwards the response to the Chithi desktop
app on your own machine. No Chithi-operated application server receives
or processes the code, and PKCE prevents it from being redeemed without
the verifier held by Chithi.

## Your rights

Under GDPR and UK GDPR you have the following rights with respect to
personal data a controller holds about you:

- **Right of access**: to request a copy of the personal data held
  about you.
- **Right to rectification**: to ask the controller to correct
  inaccurate or incomplete personal data.
- **Right to erasure**, sometimes called the right to be forgotten:
  to ask the controller to delete personal data held about you.
- **Right to restriction of processing**: to ask the controller to
  limit how it uses your personal data.
- **Right to data portability**: to receive a machine-readable copy
  of personal data you have provided.
- **Right to object**: to object to processing carried out on the
  basis of legitimate interests or for direct marketing.
- **Right to withdraw consent** at any time, where processing relies
  on consent.
- **Right to lodge a complaint** with a data-protection supervisory
  authority. In Sweden this is the
  [Integritetsskyddsmyndigheten (IMY)](https://www.imy.se).

Chithi keeps its local application data under your user profile. Removing
an account deletes its local database records and attempts to delete its
mail directory, but
uninstalling a desktop package does not necessarily remove application
data, preferences, or keyring entries. Chithi does not currently provide
a general **Clear cache** action. Follow the
[data-removal guide](user/data-removal.md) if you want to remove all
local data. OpenPGP keys use a separate, shared Tumpa keystore and must
not be deleted unless you intend to remove them from every application
that uses that keystore.

To exercise rights against data held by your mail, calendar,
contacts, or video-conferencing provider, please contact that
provider directly. They are the data controller for the data you
exchange with them through Chithi.

## How to contact us

For privacy questions or to exercise any of the rights listed above
against personal data the Chithi authors hold (for example, in
correspondence we have received from you), please open an issue at
[github.com/SUNET/chithi/issues](https://github.com/SUNET/chithi/issues)
or contact the maintainers through the contact information in the
project repository.
