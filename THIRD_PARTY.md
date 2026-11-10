# Third-party notices

The root LICENSE applies to Crystal's GPL-3.0-only code and original SVG interface icons. It does not relicense Square Enix artwork, logos, trademarks, proprietary game files, or dependencies that have their own licenses.

## Rust dependencies and external tools

Cargo.lock files record Rust dependencies and versions. Each dependency retains its own license; release builds collect their required notices into a single THIRD_PARTY_LICENSES.txt. The small files under `licenses/cargo/` supply texts omitted from some upstream crate packages. Keep the generated notices with distributed builds. Projects consulted only as technical references are not bundled dependencies. Windower, Ashita, and dgVoodoo are separate projects with their own distribution terms; their executables are not included in this source tree.

The optional Windows installer is built with [NSIS](https://nsis.sourceforge.io/License) using its zlib compressor.

## Square Enix materials

The `assets/portal-wordmark-*` images and `assets/sky/crowd-mask.png` (the party silhouette cut from the FFXI portal art) contain FINAL FANTASY XI artwork and logos belonging to Square Enix. These assets and news text and images loaded from PlayOnline at runtime are excluded from Crystal's code license. News parser test fixtures contain independently authored fictional material. The copyright and trademark notices appear in README.md and in the launcher. Square Enix's [FFXI Materials Usage License](https://support.na.square-enix.com/rule.php?id=20&la=1&tag=authc) describes its conditions for community uses; it is not a blanket license for software redistribution.

FFXI and PlayOnline executables, DLLs, DAT files, installer archives, music, and account data are not supplied in this repository. Crystal downloads official installation and update files directly from Square Enix to the user's chosen installation. Those files remain subject to Square Enix's terms.

## Fonts

The interface bundles these fonts under the SIL Open Font License, Version 1.1. Their own license files are in `licenses/fonts/`; the license text follows.

- Source Sans 3 (`assets/fonts/SourceSans3-*.ttf`): Copyright 2010-2024 Adobe (http://www.adobe.com/), with Reserved Font Name 'Source'. All Rights Reserved. Source is a trademark of Adobe in the United States and/or other countries.
- Source Serif 4 (`assets/fonts/SourceSerif4-It.ttf`): Copyright 2014 - 2023 Adobe (http://www.adobe.com/), with Reserved Font Name 'Source'. All Rights Reserved. Source is a trademark of Adobe in the United States and/or other countries.
- JetBrains Mono (`assets/fonts/JetBrainsMono-Medium.ttf`): Copyright 2020 The JetBrains Mono Project Authors (https://github.com/JetBrains/JetBrainsMono).

```text
-----------------------------------------------------------
SIL OPEN FONT LICENSE Version 1.1 - 26 February 2007
-----------------------------------------------------------

PREAMBLE
The goals of the Open Font License (OFL) are to stimulate worldwide
development of collaborative font projects, to support the font creation
efforts of academic and linguistic communities, and to provide a free and
open framework in which fonts may be shared and improved in partnership
with others.

The OFL allows the licensed fonts to be used, studied, modified and
redistributed freely as long as they are not sold by themselves. The
fonts, including any derivative works, can be bundled, embedded, 
redistributed and/or sold with any software provided that any reserved
names are not used by derivative works. The fonts and derivatives,
however, cannot be released under any other type of license. The
requirement for fonts to remain under this license does not apply
to any document created using the fonts or their derivatives.

DEFINITIONS
"Font Software" refers to the set of files released by the Copyright
Holder(s) under this license and clearly marked as such. This may
include source files, build scripts and documentation.

"Reserved Font Name" refers to any names specified as such after the
copyright statement(s).

"Original Version" refers to the collection of Font Software components as
distributed by the Copyright Holder(s).

"Modified Version" refers to any derivative made by adding to, deleting,
or substituting -- in part or in whole -- any of the components of the
Original Version, by changing formats or by porting the Font Software to a
new environment.

"Author" refers to any designer, engineer, programmer, technical
writer or other person who contributed to the Font Software.

PERMISSION & CONDITIONS
Permission is hereby granted, free of charge, to any person obtaining
a copy of the Font Software, to use, study, copy, merge, embed, modify,
redistribute, and sell modified and unmodified copies of the Font
Software, subject to the following conditions:

1) Neither the Font Software nor any of its individual components,
in Original or Modified Versions, may be sold by itself.

2) Original or Modified Versions of the Font Software may be bundled,
redistributed and/or sold with any software, provided that each copy
contains the above copyright notice and this license. These can be
included either as stand-alone text files, human-readable headers or
in the appropriate machine-readable metadata fields within text or
binary files as long as those fields can be easily viewed by the user.

3) No Modified Version of the Font Software may use the Reserved Font
Name(s) unless explicit written permission is granted by the corresponding
Copyright Holder. This restriction only applies to the primary font name as
presented to the users.

4) The name(s) of the Copyright Holder(s) or the Author(s) of the Font
Software shall not be used to promote, endorse or advertise any
Modified Version, except to acknowledge the contribution(s) of the
Copyright Holder(s) and the Author(s) or with their explicit written
permission.

5) The Font Software, modified or unmodified, in part or in whole,
must be distributed entirely under this license, and must not be
distributed under any other license. The requirement for fonts to
remain under this license does not apply to any document created
using the Font Software.

TERMINATION
This license becomes null and void if any of the above conditions are
not met.

DISCLAIMER
THE FONT SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND,
EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO ANY WARRANTIES OF
MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT
OF COPYRIGHT, PATENT, TRADEMARK, OR OTHER RIGHT. IN NO EVENT SHALL THE
COPYRIGHT HOLDER BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER LIABILITY,
INCLUDING ANY GENERAL, SPECIAL, INDIRECT, INCIDENTAL, OR CONSEQUENTIAL
DAMAGES, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING
FROM, OUT OF THE USE OR INABILITY TO USE THE FONT SOFTWARE OR FROM
OTHER DEALINGS IN THE FONT SOFTWARE.
```
