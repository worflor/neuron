# Third-party notices

This file is the inventory of third-party material that Neuron bundles into its
built binaries, and the notices that material requires. Neuron's own license is
in [`LICENSE.md`](LICENSE.md); the terms below are additional to it, not a
replacement for it. Every item here keeps its own upstream license; nothing in
this file relicenses anything.

This file was compiled by reading the workspace `Cargo.toml` files, the
`Cargo.lock` resolution, `crates/neuron-core/build.rs`,
`crates/neuron-core/src/macros/pyruntime.rs`, and the cached
`python-build-standalone` tarballs under `vendor/pbs-cache/`. No prior
third-party audit was found in the repository (`.local/license-migration/`
does not exist as of this writing), so this is the first version of this
inventory.

## 1. The embedded Python runtime (heaviest obligation)

`neuron` (the `neuron-core` crate, shared by `neuron-cli` and `neuron-app`)
bundles a private CPython interpreter directly inside the shipped executable.
This is not an optional or dynamically-loaded dependency: the interpreter
tarball is embedded with `include_bytes!` and extracted to disk on first run.
This is the one third-party component that ships as bytes inside the Neuron
binary itself, so it carries the heaviest distribution obligation in this file.

**Where it's embedded:** `crates/neuron-core/src/macros/pyruntime.rs:38`,
`static PY_TARBALL: &[u8] = include_bytes!(env!("NEURON_PY_TARBALL"));`
`build.rs` (`crates/neuron-core/build.rs`) downloads, sha256-verifies (against
the release's published `SHA256SUMS` manifest), and caches the source tarball,
then strips it (`ensure_slim` / `slim_tarball`, dropping `.pdb` symbols,
`ensurepip`/`pip`/`venv`/`test`/`idlelib`/`lib2to3`/`turtledemo`, the C headers
and unix build config, and `__pycache__`) into the slim tarball that is actually
embedded.

**Exact distribution, pinned:**

- Distributor: [`astral-sh/python-build-standalone`](https://github.com/astral-sh/python-build-standalone) ("PBS")
- Release tag: `20260610`
- CPython version: `3.12.13`
- Variant: `install_only_stripped` (the minimal runtime archive, with debug
  symbols removed; PBS also ships a larger "full"/build archive with a
  `PYTHON.json` per-component licensing manifest and full build artifacts, but
  Neuron does not use that variant)
- Asset name pattern:
  `cpython-3.12.13+20260610-<triple>-install_only_stripped.tar.gz`
- Supported build targets / PBS triples: `x86_64-pc-windows-msvc`,
  `aarch64-pc-windows-msvc`, `x86_64-apple-darwin`, `aarch64-apple-darwin`,
  `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`,
  `x86_64-unknown-linux-musl`, `aarch64-unknown-linux-musl`

### CPython / PSF license

Python and its documentation are licensed under the Python Software Foundation
License Version 2. The verbatim license text shipped inside the
`install_only_stripped` tarball (at `python/LICENSE.txt` on Windows,
`python/lib/python3.12/LICENSE.txt` elsewhere; confirmed present, unpruned, in
the slim tarball Neuron actually embeds) is reproduced below in full, exactly as
shipped:

```
A. HISTORY OF THE SOFTWARE
==========================

Python was created in the early 1990s by Guido van Rossum at Stichting
Mathematisch Centrum (CWI, see https://www.cwi.nl) in the Netherlands
as a successor of a language called ABC.  Guido remains Python's
principal author, although it includes many contributions from others.

In 1995, Guido continued his work on Python at the Corporation for
National Research Initiatives (CNRI, see https://www.cnri.reston.va.us)
in Reston, Virginia where he released several versions of the
software.

In May 2000, Guido and the Python core development team moved to
BeOpen.com to form the BeOpen PythonLabs team.  In October of the same
year, the PythonLabs team moved to Digital Creations, which became
Zope Corporation.  In 2001, the Python Software Foundation (PSF, see
https://www.python.org/psf/) was formed, a non-profit organization
created specifically to own Python-related Intellectual Property.
Zope Corporation was a sponsoring member of the PSF.

All Python releases are Open Source (see https://opensource.org for
the Open Source Definition).  Historically, most, but not all, Python
releases have also been GPL-compatible; the table below summarizes
the various releases.

    Release         Derived     Year        Owner       GPL-
                    from                                compatible? (1)

    0.9.0 thru 1.2              1991-1995   CWI         yes
    1.3 thru 1.5.2  1.2         1995-1999   CNRI        yes
    1.6             1.5.2       2000        CNRI        no
    2.0             1.6         2000        BeOpen.com  no
    1.6.1           1.6         2001        CNRI        yes (2)
    2.1             2.0+1.6.1   2001        PSF         no
    2.0.1           2.0+1.6.1   2001        PSF         yes
    2.1.1           2.1+2.0.1   2001        PSF         yes
    2.1.2           2.1.1       2002        PSF         yes
    2.1.3           2.1.2       2002        PSF         yes
    2.2 and above   2.1.1       2001-now    PSF         yes

Footnotes:

(1) GPL-compatible doesn't mean that we're distributing Python under
    the GPL.  All Python licenses, unlike the GPL, let you distribute
    a modified version without making your changes open source.  The
    GPL-compatible licenses make it possible to combine Python with
    other software that is released under the GPL; the others don't.

(2) According to Richard Stallman, 1.6.1 is not GPL-compatible,
    because its license has a choice of law clause.  According to
    CNRI, however, Stallman's lawyer has told CNRI's lawyer that 1.6.1
    is "not incompatible" with the GPL.

Thanks to the many outside volunteers who have worked under Guido's
direction to make these releases possible.


B. TERMS AND CONDITIONS FOR ACCESSING OR OTHERWISE USING PYTHON
===============================================================

Python software and documentation are licensed under the
Python Software Foundation License Version 2.

Starting with Python 3.8.6, examples, recipes, and other code in
the documentation are dual licensed under the PSF License Version 2
and the Zero-Clause BSD license.

Some software incorporated into Python is under different licenses.
The licenses are listed with code falling under that license.


PYTHON SOFTWARE FOUNDATION LICENSE VERSION 2
--------------------------------------------

1. This LICENSE AGREEMENT is between the Python Software Foundation
("PSF"), and the Individual or Organization ("Licensee") accessing and
otherwise using this software ("Python") in source or binary form and
its associated documentation.

2. Subject to the terms and conditions of this License Agreement, PSF hereby
grants Licensee a nonexclusive, royalty-free, world-wide license to reproduce,
analyze, test, perform and/or display publicly, prepare derivative works,
distribute, and otherwise use Python alone or in any derivative version,
provided, however, that PSF's License Agreement and PSF's notice of copyright,
i.e., "Copyright (c) 2001, 2002, 2003, 2004, 2005, 2006, 2007, 2008, 2009, 2010,
2011, 2012, 2013, 2014, 2015, 2016, 2017, 2018, 2019, 2020, 2021, 2022, 2023 Python Software Foundation;
All Rights Reserved" are retained in Python alone or in any derivative version
prepared by Licensee.

3. In the event Licensee prepares a derivative work that is based on
or incorporates Python or any part thereof, and wants to make
the derivative work available to others as provided herein, then
Licensee hereby agrees to include in any such work a brief summary of
the changes made to Python.

4. PSF is making Python available to Licensee on an "AS IS"
basis.  PSF MAKES NO REPRESENTATIONS OR WARRANTIES, EXPRESS OR
IMPLIED.  BY WAY OF EXAMPLE, BUT NOT LIMITATION, PSF MAKES NO AND
DISCLAIMS ANY REPRESENTATION OR WARRANTY OF MERCHANTABILITY OR FITNESS
FOR ANY PARTICULAR PURPOSE OR THAT THE USE OF PYTHON WILL NOT
INFRINGE ANY THIRD PARTY RIGHTS.

5. PSF SHALL NOT BE LIABLE TO LICENSEE OR ANY OTHER USERS OF PYTHON
FOR ANY INCIDENTAL, SPECIAL, OR CONSEQUENTIAL DAMAGES OR LOSS AS
A RESULT OF MODIFYING, DISTRIBUTING, OR OTHERWISE USING PYTHON,
OR ANY DERIVATIVE THEREOF, EVEN IF ADVISED OF THE POSSIBILITY THEREOF.

6. This License Agreement will automatically terminate upon a material
breach of its terms and conditions.

7. Nothing in this License Agreement shall be deemed to create any
relationship of agency, partnership, or joint venture between PSF and
Licensee.  This License Agreement does not grant permission to use PSF
trademarks or trade name in a trademark sense to endorse or promote
products or services of Licensee, or any third party.

8. By copying, installing or otherwise using Python, Licensee
agrees to be bound by the terms and conditions of this License
Agreement.


BEOPEN.COM LICENSE AGREEMENT FOR PYTHON 2.0
-------------------------------------------

BEOPEN PYTHON OPEN SOURCE LICENSE AGREEMENT VERSION 1

1. This LICENSE AGREEMENT is between BeOpen.com ("BeOpen"), having an
office at 160 Saratoga Avenue, Santa Clara, CA 95051, and the
Individual or Organization ("Licensee") accessing and otherwise using
this software in source or binary form and its associated
documentation ("the Software").

2. Subject to the terms and conditions of this BeOpen Python License
Agreement, BeOpen hereby grants Licensee a non-exclusive,
royalty-free, world-wide license to reproduce, analyze, test, perform
and/or display publicly, prepare derivative works, distribute, and
otherwise use the Software alone or in any derivative version,
provided, however, that the BeOpen Python License is retained in the
Software, alone or in any derivative version prepared by Licensee.

3. BeOpen is making the Software available to Licensee on an "AS IS"
basis.  BEOPEN MAKES NO REPRESENTATIONS OR WARRANTIES, EXPRESS OR
IMPLIED.  BY WAY OF EXAMPLE, BUT NOT LIMITATION, BEOPEN MAKES NO AND
DISCLAIMS ANY REPRESENTATION OR WARRANTY OF MERCHANTABILITY OR FITNESS
FOR ANY PARTICULAR PURPOSE OR THAT THE USE OF THE SOFTWARE WILL NOT
INFRINGE ANY THIRD PARTY RIGHTS.

4. BEOPEN SHALL NOT BE LIABLE TO LICENSEE OR ANY OTHER USERS OF THE
SOFTWARE FOR ANY INCIDENTAL, SPECIAL, OR CONSEQUENTIAL DAMAGES OR LOSS
AS A RESULT OF USING, MODIFYING OR DISTRIBUTING THE SOFTWARE, OR ANY
DERIVATIVE THEREOF, EVEN IF ADVISED OF THE POSSIBILITY THEREOF.

5. This License Agreement will automatically terminate upon a material
breach of its terms and conditions.

6. This License Agreement shall be governed by and interpreted in all
respects by the law of the State of California, excluding conflict of
law provisions.  Nothing in this License Agreement shall be deemed to
create any relationship of agency, partnership, or joint venture
between BeOpen and Licensee.  This License Agreement does not grant
permission to use BeOpen trademarks or trade names in a trademark
sense to endorse or promote products or services of Licensee, or any
third party.  As an exception, the "BeOpen Python" logos available at
http://www.pythonlabs.com/logos.html may be used according to the
permissions granted on that web page.

7. By copying, installing or otherwise using the software, Licensee
agrees to be bound by the terms and conditions of this License
Agreement.


CNRI LICENSE AGREEMENT FOR PYTHON 1.6.1
---------------------------------------

1. This LICENSE AGREEMENT is between the Corporation for National
Research Initiatives, having an office at 1895 Preston White Drive,
Reston, VA 20191 ("CNRI"), and the Individual or Organization
("Licensee") accessing and otherwise using Python 1.6.1 software in
source or binary form and its associated documentation.

2. Subject to the terms and conditions of this License Agreement, CNRI
hereby grants Licensee a nonexclusive, royalty-free, world-wide
license to reproduce, analyze, test, perform and/or display publicly,
prepare derivative works, distribute, and otherwise use Python 1.6.1
alone or in any derivative version, provided, however, that CNRI's
License Agreement and CNRI's notice of copyright, i.e., "Copyright (c)
1995-2001 Corporation for National Research Initiatives; All Rights
Reserved" are retained in Python 1.6.1 alone or in any derivative
version prepared by Licensee.  Alternately, in lieu of CNRI's License
Agreement, Licensee may substitute the following text (omitting the
quotes): "Python 1.6.1 is made available subject to the terms and
conditions in CNRI's License Agreement.  This Agreement together with
Python 1.6.1 may be located on the internet using the following
unique, persistent identifier (known as a handle): 1895.22/1013.  This
Agreement may also be obtained from a proxy server on the internet
using the following URL: http://hdl.handle.net/1895.22/1013".

3. In the event Licensee prepares a derivative work that is based on
or incorporates Python 1.6.1 or any part thereof, and wants to make
the derivative work available to others as provided herein, then
Licensee hereby agrees to include in any such work a brief summary of
the changes made to Python 1.6.1.

4. CNRI is making Python 1.6.1 available to Licensee on an "AS IS"
basis.  CNRI MAKES NO REPRESENTATIONS OR WARRANTIES, EXPRESS OR
IMPLIED.  BY WAY OF EXAMPLE, BUT NOT LIMITATION, CNRI MAKES NO AND
DISCLAIMS ANY REPRESENTATION OR WARRANTY OF MERCHANTABILITY OR FITNESS
FOR ANY PARTICULAR PURPOSE OR THAT THE USE OF PYTHON 1.6.1 WILL NOT
INFRINGE ANY THIRD PARTY RIGHTS.

5. CNRI SHALL NOT BE LIABLE TO LICENSEE OR ANY OTHER USERS OF PYTHON
1.6.1 FOR ANY INCIDENTAL, SPECIAL, OR CONSEQUENTIAL DAMAGES OR LOSS AS
A RESULT OF MODIFYING, DISTRIBUTING, OR OTHERWISE USING PYTHON 1.6.1,
OR ANY DERIVATIVE THEREOF, EVEN IF ADVISED OF THE POSSIBILITY THEREOF.

6. This License Agreement will automatically terminate upon a material
breach of its terms and conditions.

7. This License Agreement shall be governed by the federal
intellectual property law of the United States, including without
limitation the federal copyright law, and, to the extent such
U.S. federal law does not apply, by the law of the Commonwealth of
Virginia, excluding Virginia's conflict of law provisions.
Notwithstanding the foregoing, with regard to derivative works based
on Python 1.6.1 that incorporate non-separable material that was
previously distributed under the GNU General Public License (GPL), the
law of the Commonwealth of Virginia shall govern this License
Agreement only as to issues arising under or with respect to
Paragraphs 4, 5, and 7 of this License Agreement.  Nothing in this
License Agreement shall be deemed to create any relationship of
agency, partnership, or joint venture between CNRI and Licensee.  This
License Agreement does not grant permission to use CNRI trademarks or
trade name in a trademark sense to endorse or promote products or
services of Licensee, or any third party.

8. By clicking on the "ACCEPT" button where indicated, or by copying,
installing or otherwise using Python 1.6.1, Licensee agrees to be
bound by the terms and conditions of this License Agreement.

        ACCEPT


CWI LICENSE AGREEMENT FOR PYTHON 0.9.0 THROUGH 1.2
--------------------------------------------------

Copyright (c) 1991 - 1995, Stichting Mathematisch Centrum Amsterdam,
The Netherlands.  All rights reserved.

Permission to use, copy, modify, and distribute this software and its
documentation for any purpose and without fee is hereby granted,
provided that the above copyright notice appear in all copies and that
both that copyright notice and this permission notice appear in
supporting documentation, and that the name of Stichting Mathematisch
Centrum or CWI not be used in advertising or publicity pertaining to
distribution of the software without specific, written prior
permission.

STICHTING MATHEMATISCH CENTRUM DISCLAIMS ALL WARRANTIES WITH REGARD TO
THIS SOFTWARE, INCLUDING ALL IMPLIED WARRANTIES OF MERCHANTABILITY AND
FITNESS, IN NO EVENT SHALL STICHTING MATHEMATISCH CENTRUM BE LIABLE
FOR ANY SPECIAL, INDIRECT OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT
OF OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.

ZERO-CLAUSE BSD LICENSE FOR CODE IN THE PYTHON DOCUMENTATION
----------------------------------------------------------------------

Permission to use, copy, modify, and/or distribute this software for any
purpose with or without fee is hereby granted.

THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES WITH
REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF MERCHANTABILITY
AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR ANY SPECIAL, DIRECT,
INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES WHATSOEVER RESULTING FROM
LOSS OF USE, DATA OR PROFITS, WHETHER IN AN ACTION OF CONTRACT, NEGLIGENCE OR
OTHER TORTIOUS ACTION, ARISING OUT OF OR IN CONNECTION WITH THE USE OR
PERFORMANCE OF THIS SOFTWARE.



Additional Conditions for this Windows binary build
---------------------------------------------------

This program is linked with and uses Microsoft Distributable Code,
copyrighted by Microsoft Corporation. The Microsoft Distributable Code
is embedded in each .exe, .dll and .pyd file as a result of running
the code through a linker.

If you further distribute programs that include the Microsoft
Distributable Code, you must comply with the restrictions on
distribution specified by Microsoft. In particular, you must require
distributors and external end users to agree to terms that protect the
Microsoft Distributable Code at least as much as Microsoft's own
requirements for the Distributable Code. See Microsoft's documentation
(included in its developer tools and on its website at microsoft.com)
for specific details.

Redistribution of the Windows binary build of the Python interpreter
complies with this agreement, provided that you do not:

- alter any copyright, trademark or patent notice in Microsoft's
Distributable Code;

- use Microsoft's trademarks in your programs' names or in a way that
suggests your programs come from or are endorsed by Microsoft;

- distribute Microsoft's Distributable Code to run on a platform other
than Microsoft operating systems, run-time technologies or application
platforms; or

- include Microsoft Distributable Code in malicious, deceptive or
unlawful programs.

These restrictions apply only to the Microsoft Distributable Code as
defined above, not to Python itself or any programs running on the
Python interpreter. The redistribution of the Python interpreter and
libraries is governed by the Python Software License included with this
file, or by other licenses as marked.



--------------------------------------------------------------------------

This program, "bzip2", the associated library "libbzip2", and all
documentation, are copyright (C) 1996-2019 Julian R Seward.  All
rights reserved.

Redistribution and use in source and binary forms, with or without
modification, are permitted provided that the following conditions
are met:

1. Redistributions of source code must retain the above copyright
   notice, this list of conditions and the following disclaimer.

2. The origin of this software must not be misrepresented; you must 
   not claim that you wrote the original software.  If you use this 
   software in a product, an acknowledgment in the product 
   documentation would be appreciated but is not required.

3. Altered source versions must be plainly marked as such, and must
   not be misrepresented as being the original software.

4. The name of the author may not be used to endorse or promote 
   products derived from this software without specific prior written 
   permission.

THIS SOFTWARE IS PROVIDED BY THE AUTHOR ``AS IS'' AND ANY EXPRESS
OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE IMPLIED
WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE
ARE DISCLAIMED.  IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR ANY
DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL
DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE
GOODS OR SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS
INTERRUPTION) HOWEVER CAUSED AND ON ANY THEORY OF LIABILITY,
WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT (INCLUDING
NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE OF THIS
SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.

Julian Seward, jseward@acm.org
bzip2/libbzip2 version 1.0.8 of 13 July 2019

--------------------------------------------------------------------------

This software is copyrighted by the Regents of the University of
California, Sun Microsystems, Inc., Scriptics Corporation, ActiveState
Corporation and other parties.  The following terms apply to all files
associated with the software unless explicitly disclaimed in
individual files.

The authors hereby grant permission to use, copy, modify, distribute,
and license this software and its documentation for any purpose, provided
that existing copyright notices are retained in all copies and that this
notice is included verbatim in any distributions. No written agreement,
license, or royalty fee is required for any of the authorized uses.
Modifications to this software may be copyrighted by their authors
and need not follow the licensing terms described here, provided that
the new terms are clearly indicated on the first page of each file where
they apply.

IN NO EVENT SHALL THE AUTHORS OR DISTRIBUTORS BE LIABLE TO ANY PARTY
FOR DIRECT, INDIRECT, SPECIAL, INCIDENTAL, OR CONSEQUENTIAL DAMAGES
ARISING OUT OF THE USE OF THIS SOFTWARE, ITS DOCUMENTATION, OR ANY
DERIVATIVES THEREOF, EVEN IF THE AUTHORS HAVE BEEN ADVISED OF THE
POSSIBILITY OF SUCH DAMAGE.

THE AUTHORS AND DISTRIBUTORS SPECIFICALLY DISCLAIM ANY WARRANTIES,
INCLUDING, BUT NOT LIMITED TO, THE IMPLIED WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE, AND NON-INFRINGEMENT.  THIS SOFTWARE
IS PROVIDED ON AN "AS IS" BASIS, AND THE AUTHORS AND DISTRIBUTORS HAVE
NO OBLIGATION TO PROVIDE MAINTENANCE, SUPPORT, UPDATES, ENHANCEMENTS, OR
MODIFICATIONS.

GOVERNMENT USE: If you are acquiring this software on behalf of the
U.S. government, the Government shall have only "Restricted Rights"
in the software and related documentation as defined in the Federal
Acquisition Regulations (FARs) in Clause 52.227.19 (c) (2).  If you
are acquiring the software on behalf of the Department of Defense, the
software shall be classified as "Commercial Computer Software" and the
Government shall have only "Restricted Rights" as defined in Clause
252.227-7014 (b) (3) of DFARs.  Notwithstanding the foregoing, the
authors grant the U.S. Government and others acting in its behalf
permission to use and distribute the software in accordance with the
terms specified in this license.

This software is copyrighted by the Regents of the University of
California, Sun Microsystems, Inc., Scriptics Corporation, ActiveState
Corporation, Apple Inc. and other parties.  The following terms apply to
all files associated with the software unless explicitly disclaimed in
individual files.

The authors hereby grant permission to use, copy, modify, distribute,
and license this software and its documentation for any purpose, provided
that existing copyright notices are retained in all copies and that this
notice is included verbatim in any distributions. No written agreement,
license, or royalty fee is required for any of the authorized uses.
Modifications to this software may be copyrighted by their authors
and need not follow the licensing terms described here, provided that
the new terms are clearly indicated on the first page of each file where
they apply.

IN NO EVENT SHALL THE AUTHORS OR DISTRIBUTORS BE LIABLE TO ANY PARTY
FOR DIRECT, INDIRECT, SPECIAL, INCIDENTAL, OR CONSEQUENTIAL DAMAGES
ARISING OUT OF THE USE OF THIS SOFTWARE, ITS DOCUMENTATION, OR ANY
DERIVATIVES THEREOF, EVEN IF THE AUTHORS HAVE BEEN ADVISED OF THE
POSSIBILITY OF SUCH DAMAGE.

THE AUTHORS AND DISTRIBUTORS SPECIFICALLY DISCLAIM ANY WARRANTIES,
INCLUDING, BUT NOT LIMITED TO, THE IMPLIED WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE, AND NON-INFRINGEMENT.  THIS SOFTWARE
IS PROVIDED ON AN "AS IS" BASIS, AND THE AUTHORS AND DISTRIBUTORS HAVE
NO OBLIGATION TO PROVIDE MAINTENANCE, SUPPORT, UPDATES, ENHANCEMENTS, OR
MODIFICATIONS.

GOVERNMENT USE: If you are acquiring this software on behalf of the
U.S. government, the Government shall have only "Restricted Rights"
in the software and related documentation as defined in the Federal
Acquisition Regulations (FARs) in Clause 52.227.19 (c) (2).  If you
are acquiring the software on behalf of the Department of Defense, the
software shall be classified as "Commercial Computer Software" and the
Government shall have only "Restricted Rights" as defined in Clause
252.227-7013 (b) (3) of DFARs.  Notwithstanding the foregoing, the
authors grant the U.S. Government and others acting in its behalf
permission to use and distribute the software in accordance with the
terms specified in this license.

Copyright (c) 1993-1999 Ioi Kim Lam.
Copyright (c) 2000-2001 Tix Project Group.
Copyright (c) 2004 ActiveState

This software is copyrighted by the above entities
and other parties.  The following terms apply to all files associated
with the software unless explicitly disclaimed in individual files.

The authors hereby grant permission to use, copy, modify, distribute,
and license this software and its documentation for any purpose, provided
that existing copyright notices are retained in all copies and that this
notice is included verbatim in any distributions. No written agreement,
license, or royalty fee is required for any of the authorized uses.
Modifications to this software may be copyrighted by their authors
and need not follow the licensing terms described here, provided that
the new terms are clearly indicated on the first page of each file where
they apply.

IN NO EVENT SHALL THE AUTHORS OR DISTRIBUTORS BE LIABLE TO ANY PARTY
FOR DIRECT, INDIRECT, SPECIAL, INCIDENTAL, OR CONSEQUENTIAL DAMAGES
ARISING OUT OF THE USE OF THIS SOFTWARE, ITS DOCUMENTATION, OR ANY
DERIVATIVES THEREOF, EVEN IF THE AUTHORS HAVE BEEN ADVISED OF THE
POSSIBILITY OF SUCH DAMAGE.

THE AUTHORS AND DISTRIBUTORS SPECIFICALLY DISCLAIM ANY WARRANTIES,
INCLUDING, BUT NOT LIMITED TO, THE IMPLIED WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE, AND NON-INFRINGEMENT.  THIS SOFTWARE
IS PROVIDED ON AN "AS IS" BASIS, AND THE AUTHORS AND DISTRIBUTORS HAVE
NO OBLIGATION TO PROVIDE MAINTENANCE, SUPPORT, UPDATES, ENHANCEMENTS, OR
MODIFICATIONS.

GOVERNMENT USE: If you are acquiring this software on behalf of the
U.S. government, the Government shall have only "Restricted Rights"
in the software and related documentation as defined in the Federal 
Acquisition Regulations (FARs) in Clause 52.227.19 (c) (2).  If you
are acquiring the software on behalf of the Department of Defense, the
software shall be classified as "Commercial Computer Software" and the
Government shall have only "Restricted Rights" as defined in Clause
252.227-7013 (c) (1) of DFARs.  Notwithstanding the foregoing, the
authors grant the U.S. Government and others acting in its behalf
permission to use and distribute the software in accordance with the
terms specified in this license. 

----------------------------------------------------------------------

Parts of this software are based on the Tcl/Tk software copyrighted by
the Regents of the University of California, Sun Microsystems, Inc.,
and other parties. The original license terms of the Tcl/Tk software
distribution is included in the file docs/license.tcltk.

Parts of this software are based on the HTML Library software
copyrighted by Sun Microsystems, Inc. The original license terms of
the HTML Library software distribution is included in the file
docs/license.html_lib.
```

(That is the complete `python/LICENSE.txt` as shipped, reproduced without
edit: the PSF license followed by the licenses for the software incorporated
into CPython, including bzip2/libbzip2 and Tcl/Tk. Nothing is condensed,
summarised, or re-ordered. The same file, plus
`python/tcl/tk8.6/license.terms`, ships inside the interpreter tree that
Neuron unpacks at first use.)

### Other components linked into the bundled interpreter

The `install_only_stripped` tarball (unlike PBS's larger "full" archive) does not ship
a `PYTHON.json` per-component licensing manifest. Cargo cannot resolve
licenses for material that isn't a Rust crate, so the components below were
identified directly from the tarball's own file listing
(`python/DLLs/*.dll`, confirmed present in both the upstream and the
build.rs-slimmed tarball Neuron embeds) rather than assumed:

| component | evidence in the tarball | license (upstream, not separately shipped as a text file in this tarball) |
| --- | --- | --- |
| OpenSSL 3.5.7 | `libcrypto-3-x64.dll`, `libssl-3-x64.dll`, the `_ssl` module | Apache License 2.0 (verbatim below) |
| SQLite 3.53.1 | `sqlite3.dll`, the `_sqlite3` module / `Lib/sqlite3` | public domain (no text to reproduce) |
| libffi 8 (upstream 3.4.x) | `libffi-8.dll`, the `_ctypes` module | MIT-style, libffi's own license (verbatim below) |
| Tcl/Tk | `tcl86t.dll`, `tk86t.dll`, `python/tcl/tk8.6/license.terms` | BSD-style (see verbatim text above) |
| bzip2 | `_bz2` module (statically linked) | BSD-style (see verbatim text above) |
| zlib | `zlib_codec` / statically linked into `python312.dll` | zlib License |
| xz / liblzma | `_lzma` module (statically linked) | public domain |

PBS's own documentation (`running.html#licensing` on the project's docs site)
states that Python's dependencies "are governed by varied software use
licenses" that are "fairly permissive," and that it avoids GPL-licensed
components (readline, GDBM) specifically by substituting `libedit` and
disabling `_gdbm`, so nothing GPL-only enters the bundle through CPython
itself. python-build-standalone (the build tooling/project, as distinct from
the CPython binaries it produces) is itself released under MPL-2.0; Neuron
does not modify or redistribute that tooling, only its output artifact.

**Resolved.** The `install_only_stripped` variant does not carry individual upstream
LICENSE files for OpenSSL, SQLite, or libffi the way it does for bzip2 and
Tcl/Tk (those two are folded into `python/LICENSE.txt`), so the required texts
are reproduced verbatim below from each project's own release, at the version
the bundled interpreter actually ships. Versions were read from the shipped
DLLs' own version resources, not assumed: OpenSSL 3.5.7
(`libcrypto-3-x64.dll`, `libssl-3-x64.dll`), SQLite 3.53.1 (`sqlite3.dll`),
libffi 8 (`libffi-8.dll`). SQLite is public domain and has no license text to
reproduce.

#### OpenSSL 3.5.7, Apache License 2.0

Verbatim from the `openssl-3.5.7` tag's `LICENSE.txt`:

```

                                 Apache License
                           Version 2.0, January 2004
                        https://www.apache.org/licenses/

   TERMS AND CONDITIONS FOR USE, REPRODUCTION, AND DISTRIBUTION

   1. Definitions.

      "License" shall mean the terms and conditions for use, reproduction,
      and distribution as defined by Sections 1 through 9 of this document.

      "Licensor" shall mean the copyright owner or entity authorized by
      the copyright owner that is granting the License.

      "Legal Entity" shall mean the union of the acting entity and all
      other entities that control, are controlled by, or are under common
      control with that entity. For the purposes of this definition,
      "control" means (i) the power, direct or indirect, to cause the
      direction or management of such entity, whether by contract or
      otherwise, or (ii) ownership of fifty percent (50%) or more of the
      outstanding shares, or (iii) beneficial ownership of such entity.

      "You" (or "Your") shall mean an individual or Legal Entity
      exercising permissions granted by this License.

      "Source" form shall mean the preferred form for making modifications,
      including but not limited to software source code, documentation
      source, and configuration files.

      "Object" form shall mean any form resulting from mechanical
      transformation or translation of a Source form, including but
      not limited to compiled object code, generated documentation,
      and conversions to other media types.

      "Work" shall mean the work of authorship, whether in Source or
      Object form, made available under the License, as indicated by a
      copyright notice that is included in or attached to the work
      (an example is provided in the Appendix below).

      "Derivative Works" shall mean any work, whether in Source or Object
      form, that is based on (or derived from) the Work and for which the
      editorial revisions, annotations, elaborations, or other modifications
      represent, as a whole, an original work of authorship. For the purposes
      of this License, Derivative Works shall not include works that remain
      separable from, or merely link (or bind by name) to the interfaces of,
      the Work and Derivative Works thereof.

      "Contribution" shall mean any work of authorship, including
      the original version of the Work and any modifications or additions
      to that Work or Derivative Works thereof, that is intentionally
      submitted to Licensor for inclusion in the Work by the copyright owner
      or by an individual or Legal Entity authorized to submit on behalf of
      the copyright owner. For the purposes of this definition, "submitted"
      means any form of electronic, verbal, or written communication sent
      to the Licensor or its representatives, including but not limited to
      communication on electronic mailing lists, source code control systems,
      and issue tracking systems that are managed by, or on behalf of, the
      Licensor for the purpose of discussing and improving the Work, but
      excluding communication that is conspicuously marked or otherwise
      designated in writing by the copyright owner as "Not a Contribution."

      "Contributor" shall mean Licensor and any individual or Legal Entity
      on behalf of whom a Contribution has been received by Licensor and
      subsequently incorporated within the Work.

   2. Grant of Copyright License. Subject to the terms and conditions of
      this License, each Contributor hereby grants to You a perpetual,
      worldwide, non-exclusive, no-charge, royalty-free, irrevocable
      copyright license to reproduce, prepare Derivative Works of,
      publicly display, publicly perform, sublicense, and distribute the
      Work and such Derivative Works in Source or Object form.

   3. Grant of Patent License. Subject to the terms and conditions of
      this License, each Contributor hereby grants to You a perpetual,
      worldwide, non-exclusive, no-charge, royalty-free, irrevocable
      (except as stated in this section) patent license to make, have made,
      use, offer to sell, sell, import, and otherwise transfer the Work,
      where such license applies only to those patent claims licensable
      by such Contributor that are necessarily infringed by their
      Contribution(s) alone or by combination of their Contribution(s)
      with the Work to which such Contribution(s) was submitted. If You
      institute patent litigation against any entity (including a
      cross-claim or counterclaim in a lawsuit) alleging that the Work
      or a Contribution incorporated within the Work constitutes direct
      or contributory patent infringement, then any patent licenses
      granted to You under this License for that Work shall terminate
      as of the date such litigation is filed.

   4. Redistribution. You may reproduce and distribute copies of the
      Work or Derivative Works thereof in any medium, with or without
      modifications, and in Source or Object form, provided that You
      meet the following conditions:

      (a) You must give any other recipients of the Work or
          Derivative Works a copy of this License; and

      (b) You must cause any modified files to carry prominent notices
          stating that You changed the files; and

      (c) You must retain, in the Source form of any Derivative Works
          that You distribute, all copyright, patent, trademark, and
          attribution notices from the Source form of the Work,
          excluding those notices that do not pertain to any part of
          the Derivative Works; and

      (d) If the Work includes a "NOTICE" text file as part of its
          distribution, then any Derivative Works that You distribute must
          include a readable copy of the attribution notices contained
          within such NOTICE file, excluding those notices that do not
          pertain to any part of the Derivative Works, in at least one
          of the following places: within a NOTICE text file distributed
          as part of the Derivative Works; within the Source form or
          documentation, if provided along with the Derivative Works; or,
          within a display generated by the Derivative Works, if and
          wherever such third-party notices normally appear. The contents
          of the NOTICE file are for informational purposes only and
          do not modify the License. You may add Your own attribution
          notices within Derivative Works that You distribute, alongside
          or as an addendum to the NOTICE text from the Work, provided
          that such additional attribution notices cannot be construed
          as modifying the License.

      You may add Your own copyright statement to Your modifications and
      may provide additional or different license terms and conditions
      for use, reproduction, or distribution of Your modifications, or
      for any such Derivative Works as a whole, provided Your use,
      reproduction, and distribution of the Work otherwise complies with
      the conditions stated in this License.

   5. Submission of Contributions. Unless You explicitly state otherwise,
      any Contribution intentionally submitted for inclusion in the Work
      by You to the Licensor shall be under the terms and conditions of
      this License, without any additional terms or conditions.
      Notwithstanding the above, nothing herein shall supersede or modify
      the terms of any separate license agreement you may have executed
      with Licensor regarding such Contributions.

   6. Trademarks. This License does not grant permission to use the trade
      names, trademarks, service marks, or product names of the Licensor,
      except as required for reasonable and customary use in describing the
      origin of the Work and reproducing the content of the NOTICE file.

   7. Disclaimer of Warranty. Unless required by applicable law or
      agreed to in writing, Licensor provides the Work (and each
      Contributor provides its Contributions) on an "AS IS" BASIS,
      WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or
      implied, including, without limitation, any warranties or conditions
      of TITLE, NON-INFRINGEMENT, MERCHANTABILITY, or FITNESS FOR A
      PARTICULAR PURPOSE. You are solely responsible for determining the
      appropriateness of using or redistributing the Work and assume any
      risks associated with Your exercise of permissions under this License.

   8. Limitation of Liability. In no event and under no legal theory,
      whether in tort (including negligence), contract, or otherwise,
      unless required by applicable law (such as deliberate and grossly
      negligent acts) or agreed to in writing, shall any Contributor be
      liable to You for damages, including any direct, indirect, special,
      incidental, or consequential damages of any character arising as a
      result of this License or out of the use or inability to use the
      Work (including but not limited to damages for loss of goodwill,
      work stoppage, computer failure or malfunction, or any and all
      other commercial damages or losses), even if such Contributor
      has been advised of the possibility of such damages.

   9. Accepting Warranty or Additional Liability. While redistributing
      the Work or Derivative Works thereof, You may choose to offer,
      and charge a fee for, acceptance of support, warranty, indemnity,
      or other liability obligations and/or rights consistent with this
      License. However, in accepting such obligations, You may act only
      on Your own behalf and on Your sole responsibility, not on behalf
      of any other Contributor, and only if You agree to indemnify,
      defend, and hold each Contributor harmless for any liability
      incurred by, or claims asserted against, such Contributor by reason
      of your accepting any such warranty or additional liability.

   END OF TERMS AND CONDITIONS
```

#### libffi, MIT-style

Verbatim from libffi's `LICENSE` (v3.4.6, the 3.4.x series `libffi-8.dll`
comes from):

```
libffi - Copyright (c) 1996-2024  Anthony Green, Red Hat, Inc and others.
See source files for details.

Permission is hereby granted, free of charge, to any person obtaining
a copy of this software and associated documentation files (the
``Software''), to deal in the Software without restriction, including
without limitation the rights to use, copy, modify, merge, publish,
distribute, sublicense, and/or sell copies of the Software, and to
permit persons to whom the Software is furnished to do so, subject to
the following conditions:

The above copyright notice and this permission notice shall be
included in all copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED ``AS IS'', WITHOUT WARRANTY OF ANY KIND,
EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF
MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT.
IN NO EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY
CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION OF CONTRACT,
TORT OR OTHERWISE, ARISING FROM, OUT OF OR IN CONNECTION WITH THE
SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.
```

#### SQLite 3.53.1, public domain

SQLite's authors dedicated it to the public domain, so there is no copyright
holder to attribute and no notice condition to satisfy; there is deliberately
no license file to vendor. Upstream states this at `sqlite.org/copyright.html`.

## 2. Rust dependencies

Read from each crate's `Cargo.toml` (direct, declared dependencies only) and
cross-referenced against the exact resolved version and license field in the
local Cargo registry cache (`~/.cargo/registry/src/.../<pkg>-<version>/Cargo.toml`),
not assumed from memory. `cargo` itself was not invoked to produce this table.

| crate | resolved version | license | used by |
| --- | --- | --- | --- |
| slint | 1.16.1 | `GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR LicenseRef-Slint-Software-3.0` | neuron-app (UI toolkit), see §3 |
| slint-build | 1.16.1 | same as slint | neuron-app (build-dependency) |
| tray-icon | 0.24.0 | MIT OR Apache-2.0 | neuron-app (system tray) |
| global-hotkey | 0.8.0 | Apache-2.0 OR MIT | neuron-app (OS-wide hotkeys) |
| image | 0.25.10 | MIT OR Apache-2.0 | neuron-app (PNG/GIF decode) |
| cpal | 0.15.3 | Apache-2.0 | neuron-app, neuron-core (audio output) |
| serde | 1.0.228 | MIT OR Apache-2.0 | workspace-wide |
| serde_json | 1.0.150 | MIT OR Apache-2.0 | workspace-wide |
| toml | 0.8.2 | MIT OR Apache-2.0 | workspace-wide (config format) |
| clap | 4.6.1 | MIT OR Apache-2.0 | neuron-cli |
| anyhow | 1.0.102 | MIT OR Apache-2.0 | workspace-wide |
| zip | 2.4.2 | MIT | neuron-core (Synapse export unzip) |
| quick-xml | 0.36.2 | MIT | neuron-core (Synapse export XML parse) |
| windows-sys | 0.59.0 | MIT OR Apache-2.0 | neuron-core, neuron-app, neuron-cli, neuron-host, neuron-testkit (Win32 FFI) |
| windows | 0.54.0 / 0.62.2 (transitive; not a direct dependency of any crate in this workspace) | MIT OR Apache-2.0 | pulled in by tray-icon / global-hotkey |
| num-complex | 0.4.6 | MIT OR Apache-2.0 | engram |
| rayon | 1.12.0 | MIT OR Apache-2.0 | engram (parallel pair processing) |
| tar | 0.4.46 | MIT OR Apache-2.0 | neuron-core (interpreter extraction + build-time slim step) |
| flate2 | 1.1.9 | MIT OR Apache-2.0 | neuron-core, neuron-testkit (gzip) |
| dirs | 6.0.0 | MIT OR Apache-2.0 | neuron-core (per-user data dir) |
| ureq | 3.3.0 | MIT OR Apache-2.0 | neuron-core build.rs only (fetches PBS tarball at build time; not in the shipped binary) |
| sha2 | 0.10.9 | MIT OR Apache-2.0 | neuron-core build.rs only (verifies PBS tarball checksum; not in the shipped binary) |
| proptest | 1.11.0 | MIT OR Apache-2.0 | dev-dependency only (tests), not shipped |
| static_assertions | 1.1.0 | MIT OR Apache-2.0 | dev-dependency only (tests), not shipped |

Every one of these permissive (MIT/Apache-2.0/dual) licenses is compatible
with distributing the combined work under GPL-3.0-or-later: MIT and
Apache-2.0 are both one-way compatible with the GPL (they impose fewer
restrictions than the GPL, so a GPL-covered combination can satisfy both the
GPL's terms and theirs simultaneously). None of them require the combined
work to be relicensed; they do require their own copyright notice and
license text to travel with the binary, which this file provides.

`engram` itself is not third-party to Neuron (it is a Woflo Labs component
under WLCSL-1.0, already covered by the repository-root `LICENSE.md` and
`crates/engram/LICENSE.md`), so it is listed above only as a consumer of
`num-complex`/`rayon`, not as an entry needing its own third-party notice.

## 3. Slint (GPL arm)

Slint is multi-licensed upstream: a project can use it under `GPL-3.0-only`,
under Slint's own "Royalty-free" license, or under a paid Slint commercial
license. The `Cargo.toml` dependency declaration (`slint = "1.16"`) does not
by itself pick an arm. Cargo has no field for that; the choice is made by
which conditions the distributor actually satisfies.

**Neuron relies on the `GPL-3.0-only` arm.** Reasoning:

- Neuron does not hold a Slint commercial license and does not claim the
  royalty-free arm's conditions (that arm has its own restrictions unrelated
  to Neuron's distribution model); the only arm Neuron actually satisfies is
  the GPL one.
- `neuron-app` (the crate that depends on `slint`) is itself licensed
  `GPL-3.0-or-later` per the repository-root `LICENSE.md`, so building it
  against Slint under `GPL-3.0-only` is exactly the licensing model Slint's
  GPL arm is designed for: a GPL-covered application linking a GPL-covered
  UI toolkit.
- Combining a `GPL-3.0-only` dependency into a `GPL-3.0-or-later` work is a
  standard, permitted combination: `-or-later` means the downstream work may
  be distributed under GPLv3 or any later version *at the licensee's
  choice*, and distributing under plain GPLv3 (matching the dependency's
  `-only` requirement) is always one of those permitted choices. The
  combined binary is therefore distributable under GPL-3.0 terms that
  satisfy both Neuron's own license and Slint's GPL arm at once.

A GPL-only fork of Slint's arm does mean: anyone redistributing a Neuron
binary that links `slint` is redistributing a combined GPL work, and must be
able to provide (or point to) the corresponding source for the GPL-covered
parts, per the GPL's own terms: the same obligation Neuron's own
`LICENSE.md` already places on the rest of the codebase.

## 4. Assets

Neuron's own asset footprint is small, and everything found was traced to its
origin rather than assumed:

- **Tray icon.** Not a bundled image file. `crates/neuron-app/src/tray.rs`
  (`load_icon`, around line 236) generates the 32x32 RGBA tray icon
  procedurally at runtime (`tray_icon::Icon::from_rgba`): a small drawn
  rounded-square-and-diamond mark in the app's own accent color. There is no
  `include_bytes!` of a PNG/ICO anywhere in `neuron-app`. The `image` crate
  dependency (§2) is for decoding *other* runtime images (e.g. imported
  content), not this icon.
- **Fonts.** No custom font files are bundled. The UI uses Slint's default
  font handling; any font-related obligations are Slint's own (§3), not a
  separate asset.
- **`crates/neuron-host/src/adapters/chroma_shm_data/*.bin`.** Small binary
  fixtures (`keystream.bin`, a handful of `overwatch-*.bin` frame captures)
  compiled in with `include_bytes!` for the native-Chroma SHM codec. Per
  that directory's own `README.md`, these are the project's own captured
  bytes from live-probing a real game session (Overwatch, SDK 3.37,
  2026-07-03): reverse-engineering evidence, not a third-party asset with
  its own license. `keystream.bin` (a 512-byte de-obfuscation table) is used
  at runtime by the decoder, not only by tests, so it does ship inside the
  binary; it is Woflo Labs' own transcription of an observed constant, not
  someone else's copyrighted file.
- **Device definition TOMLs** (`crates/neuron-core/devices/*.toml`), the
  bundled macro exemplar (`crates/neuron-core/src/macros/defaults/beacon_demo.py`),
  and the two host scripts (`runtime/host/neuron_host.py`,
  `runtime/host/neuron.py`, embedded via `include_str!` in
  `pyruntime.rs:44-46`) all carry Woflo Labs' own
  `SPDX-License-Identifier: GPL-3.0-or-later` headers. They are original
  Neuron material, not third-party assets, and are already covered by the
  repository-root `LICENSE.md`.

## 5. Reference material that is not a dependency

The README credits two things as *references* used to work out the
`razer_report` HID protocol: raw USB wire captures taken with **USBPcap**,
and the open-source **OpenRazer** driver. Neither is a Neuron dependency,
neither is bundled into any binary, and neither is a source Neuron's code was
copied from:

- Protocol facts (byte offsets, opcode values, report shapes) are functional
  facts about how a piece of hardware communicates. Two independent
  implementations agreeing on those facts, because they both observed the
  same wire traffic or read the same device, does not establish that one
  copied the other's expression, only that the hardware behaves
  consistently.
- A targeted check of `crates/neuron-core/src/device.rs`, `registry.rs`,
  `synth.rs`, and the docs that discuss OpenRazer (`docs/PROTOCOL-HOST.md`)
  found no verbatim code, comment blocks, or tables copied from OpenRazer.
  README's own wording is explicit that opcodes were "worked out from" wire
  captures and the OpenRazer driver "and a lot of live probing", i.e.
  cross-checked against, not adapted from. **No provenance concern was found
  in this pass.** If a future contribution ever pastes OpenRazer source
  (code, not just a discovered opcode value) directly into this repository,
  that would be a real GPL-provenance question and should be caught in
  review before merge, not addressed retroactively here.
- USBPcap is a packet-capture tool the author ran locally to observe traffic;
  its own license (BSD-style, per the USBPcap project) governs the tool
  itself, and using it to observe a protocol carries no licensing
  consequence for Neuron's own code.

## 6. Source vs. binary releases

mk1 ("public beta mk1") ships as a source checkout: `cargo build --release`
is the documented install path today. Several obligations in this file only
bite once Neuron starts shipping prebuilt binaries to other people:

- The embedded-CPython notice (§1) and this file's existence at all matter
  most for a **binary** release, because that is the form in which
  copyrighted third-party bytes (the interpreter, OpenSSL, SQLite, etc.)
  actually leave this repository inside `neuron`/`neuron-app`. A source
  checkout doesn't embed anything until it's built; `build.rs` fetches and
  verifies the interpreter tarball at build time, on the builder's own
  machine.
- The Rust dependency notices (§2) and the Slint GPL-arm reasoning (§3)
  apply to *any* distributed binary linking these crates, whether that
  binary is built by the project or by a downstream builder from source. A
  from-source build by an end user for their own use does not by itself
  trigger a redistribution obligation; handing a *built* binary to someone
  else does.
- Nothing in this file changes if mk1 stays source-only for a while longer:
  it exists now so that whenever prebuilt binaries do start shipping, the
  obligations are already documented and don't need to be reconstructed
  under release pressure.
