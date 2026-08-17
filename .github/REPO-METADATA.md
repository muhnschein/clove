# Repository metadata

The parts of this project that live in GitHub's settings rather than in the
tree, written down so they are not one maintainer's memory.

## About

> A modern I2P-only BitTorrent client. Rust, SAMv3, Linux; no clearnet mode to
> misconfigure.

No website, no sponsor link. Issues on; wiki and discussions off — the manuals
in `man/` are the documentation.

## Topics

```console
$ gh repo edit muhnschein/clove \
    --add-topic i2p --add-topic i2pd --add-topic bittorrent \
    --add-topic torrent-client --add-topic p2p --add-topic samv3 \
    --add-topic rust --add-topic linux --add-topic privacy \
    --add-topic anonymity --add-topic seccomp --add-topic landlock
```

Twelve of GitHub's twenty: what it is (`bittorrent`, `torrent-client`, `p2p`),
the network it is only ever on (`i2p`, `i2pd`, `samv3`), what it is built from
and runs on (`rust`, `linux`), what it is for (`privacy`, `anonymity`), and how
it defends itself (`seccomp`, `landlock`).

## Social preview

[`social-preview.png`](social-preview.png), 1280×640, from
[`social-preview.svg`](social-preview.svg). Regenerate with any rasteriser:

```console
$ rsvg-convert -w 1280 -h 640 .github/social-preview.svg -o .github/social-preview.png
```

Uploading it has no API: **Settings → General → Social preview → Edit →
Upload an image**. Keep it under 1 MB, and keep the mark and wordmark away from
the edges — the sites that unfurl it crop to several aspect ratios.

## Labels

[`ISSUE_TEMPLATE/interoperability.yml`](ISSUE_TEMPLATE/interoperability.yml)
applies `bug`. An `interop` label would say more; create it first, since a
label named in a form but missing from the repository is dropped silently.
