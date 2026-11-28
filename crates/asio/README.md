# bitspring-asio

A small, safe ASIO host for Windows, written for BitSpring. It finds the ASIO
drivers installed on the machine, loads one, and plays PCM or native DSD
through it, bit for bit. Every driver call runs on one thread owned by this
crate, with the single-threaded COM apartment and the message pump that ASIO
drivers expect. The audio callback does no allocation and takes no locks.

On other systems the crate builds only its platform-independent parts (sample
formats, buffer size rules, DSD rates, driver id parsing), so its tests run
anywhere.

## Licence

This crate is licensed under the GNU General Public License, version 3 only
(see `LICENSE`).

The reason is its origin: the ASIO interface it binds to (the `IASIO` driver
interface, its structures and constants) is translated from the Steinberg ASIO
SDK 2.3.4 headers. Since 2025 Steinberg offers the ASIO SDK under two licences,
its own proprietary licence or, alternatively, the GPL version 3. BitSpring
uses the GPLv3 option.

Linking this crate makes the Windows build of BitSpring, as a whole, a GPLv3
work. The rest of the BitSpring repository, including every other crate and
the builds for other systems, stays under the MIT licence.

ASIO is a registered trademark of Steinberg Media Technologies GmbH.
