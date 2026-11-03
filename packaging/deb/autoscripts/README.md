# debhelper autoscript templates

These five files are the shell snippets that `dh_installsystemd` of debhelper
13.11.4 (the version in Debian 12) adds to the maintainer scripts of a service
package. `packaging/package-deb.sh` fills in their placeholders and puts them
into the `.deb`'s `postinst`, `prerm` and `postrm`, so the package handles its
systemd unit exactly as a debhelper-built package does. They are copied
unchanged and must stay that way: `package-deb.sh` checks every one against
`SHA256SUMS` and stops if a byte differs.

| File | Used in | Placeholders |
|---|---|---|
| `postinst-systemd-enable` | `postinst`, first | `#UNITFILE#` |
| `postinst-systemd-restart` | `postinst`, second | `#UNITFILES#`, `#RESTART_ACTION#` |
| `prerm-systemd-restart` | `prerm` | `#UNITFILES#` |
| `postrm-systemd-reload-only` | `postrm`, first | `#UNITFILES#` (unused by this template) |
| `postrm-systemd` | `postrm`, second | `#UNITFILES#` |

The set is the one `dh_installsystemd` 13.11.4 uses for a unit with an
`[Install]` section at compatibility level 13 with its default options (enable,
start, restart after an upgrade). Because the restart is done after the
upgrade, it uses no `preinst-systemd-stop`, so `preinst` gets no snippet.

## Origin

- Source package: `debhelper` 13.11.4, in Debian 12 (bookworm).
- Binary package that carries the files: `libdebhelper-perl_13.11.4_all.deb`
  (the `debhelper` package itself depends on it).
- Path inside it: `/usr/share/debhelper/autoscripts/`.
- SHA-256 of that `.deb`:
  `105b26fc95776db8928fe1442cc2485a969a29ea395a7639c19199a1442ef50f`
  (fetch it with `apt-get download libdebhelper-perl` in a `debian:12`
  container, or from the Debian archive).

## Licence

debhelper's `debian/copyright` has this stanza, which covers `autoscripts/*`:

```
Files: examples/* autoscripts/*
Copyright: 1997-2011 Joey Hess <joeyh@debian.org>
License: public-domain
 These files are in the public domain.
 .
 Pedants who believe I cannot legally say that code I have written is in
 the public domain may consider them instead to be licensed as follows:
 .
 Redistribution and use in source and binary forms, with or without
 modification, are permitted under any circumstances. No warranty.
```

The package's `copyright` file repeats this notice for the parts of the
maintainer scripts generated from these templates.

## Checking them against debhelper

`make deb-check` runs `packaging/deb/check-autoscripts.sh` in a `debian:12`
container. It runs the real `dh` (compatibility level 13, default options) on a
scratch package with the same unit and the same hand-written scripts, and
compares the four maintainer scripts it produces with the ones
`package-deb.sh` assembled, byte for byte. Run it after any change in
`packaging/deb/` or in the assembly code of `package-deb.sh`.
