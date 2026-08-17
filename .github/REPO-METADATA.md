# Repository metadata

The parts of this project that live in GitHub's settings rather than in the
tree: the About box, the topics, and the social preview. None of them can be
committed, so they are written down here — otherwise they are one maintainer's
memory, and they drift.

## About

Description:

> A modern I2P-only BitTorrent client. Rust, SAMv3, Linux; no clearnet mode to
> misconfigure.

No website, no sponsor link. Releases and packages are off; issues on;
discussions off; wiki off — the manuals in `man/` are the documentation.

## Topics

```console
$ gh repo edit muhnschein/clove \
    --description 'A modern I2P-only BitTorrent client. Rust, SAMv3, Linux; no clearnet mode to misconfigure.' \
    --add-topic i2p --add-topic i2pd --add-topic bittorrent \
    --add-topic torrent-client --add-topic p2p --add-topic samv3 \
    --add-topic rust --add-topic linux --add-topic privacy \
    --add-topic anonymity --add-topic seccomp --add-topic landlock
```

Twelve, of GitHub's twenty: what the thing is (`bittorrent`, `torrent-client`,
`p2p`), the network it is only ever on (`i2p`, `i2pd`, `samv3`), what it is
built from and runs on (`rust`, `linux`), what it is for (`privacy`,
`anonymity`), and how it defends itself (`seccomp`, `landlock`). Someone
searching any one of those is looking for something clove might be.

Same set through the API, if `gh` is not to hand:

```console
$ gh api -X PUT repos/muhnschein/clove/topics \
    -f names[]=i2p -f names[]=i2pd -f names[]=bittorrent \
    -f names[]=torrent-client -f names[]=p2p -f names[]=samv3 \
    -f names[]=rust -f names[]=linux -f names[]=privacy \
    -f names[]=anonymity -f names[]=seccomp -f names[]=landlock
```

Note that `PUT /topics` replaces the whole list rather than adding to it.

## Social preview

[`social-preview.png`](social-preview.png) — 1280×640, the size GitHub asks
for, and what is unfurled when a link to the repository is posted anywhere
that reads OpenGraph tags.

There is no API for it: **Settings → General → Social preview → Edit → Upload
an image**, and pick `.github/social-preview.png` out of a checkout. It
survives everything except someone replacing it.

[`social-preview.svg`](social-preview.svg) is the source. Regenerate the PNG
after editing it, with any rasteriser:

```console
$ rsvg-convert -w 1280 -h 640 .github/social-preview.svg -o .github/social-preview.png
```

Keep it under 1 MB, and keep the wordmark and the garlic away from the edges:
the preview is cropped to several aspect ratios by the sites that unfurl it.

## Labels

The interoperability form applies `bug`, because that label exists. An
`interop` label would say more — create it and change `labels:` in
[`ISSUE_TEMPLATE/interoperability.yml`](ISSUE_TEMPLATE/interoperability.yml);
a label named there that does not exist in the repository is dropped silently,
which is worse than not naming one.
