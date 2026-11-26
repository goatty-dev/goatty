# Bundled fonts

## JetBrainsMono Nerd Font (terminal)

JetBrainsMono Nerd Font (Regular, Bold, Italic, Bold Italic) from
[Nerd Fonts](https://github.com/ryanoasis/nerd-fonts) v3.5.1, unmodified.
`scripts/update-nerd-font.sh` fetches them from the pinned release and checks
its SHA-256.

goatty registers these faces at start. The terminal falls back to them when
no installed Nerd Font comes first in `font.fallbacks`, every terminal font
falls back to them for Nerd Font symbols, and tab icons always use them.

License: JetBrains Mono is under the SIL Open Font License 1.1 (`OFL.txt`).
The Nerd Font symbols added by the patcher come from icon sets under their
own permissive licenses (MIT, OFL, Apache 2.0, CC BY 4.0); see
<https://github.com/ryanoasis/nerd-fonts/blob/master/license-audit.md>.

## Inter (UI)

Inter (Regular, Medium, SemiBold, Bold: the weights goatty's UI uses) from
[Inter](https://github.com/rsms/inter) 4.1, the static TTFs in
`extras/ttf`, unmodified. `scripts/update-ui-font.sh` fetches them from the
pinned release and checks its SHA-256.

goatty registers these faces at start, and Inter is the default interface
font (`font.ui_fallbacks`). Static faces, not the variable font: GPUI's Linux
text system draws a variable font at weight 400 whatever weight is asked for
(zed#64783), so an installed variable Inter, Cantarell or Adwaita Sans shows
no bold or semibold text at all.

License: SIL Open Font License 1.1 (`Inter-OFL.txt`).
