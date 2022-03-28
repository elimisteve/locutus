# Locutus 

[![Chat on Discord](https://img.shields.io/discord/917499817758978089?label=chat&logo=discord)](https://discord.gg/Q2FWzCqKQD)

The Internet has grown increasingly centralized over the past 25 years, such that a handful of companies now effectively control the Internet infrastructure. The public square is privately owned, threatening freedom of speech and democracy. It is imperative that we provide a viable alternative to big tech, but which won't just become what it seeks to replace.

Locutus is a software platform that makes it easy to create completely decentralized alternatives to today's centralized tech companies. These decentralized apps will be easy to use, scalable, and secured through cryptography.

Locutus apps can be built with familiar tools like [React](https://reactjs.org/) or [Vue.js](https://vuejs.org/), and can be "bridged" to the legacy centralized apps they replace.

### Applications

The following applications can be built on the Locutus platform:

* Decentralized email (with a gateway to legacy email via the @freenet.org domain)
* Decentralized microblogging (think Twitter or Facebook)
* Instant Messaging (Whatsapp, Signal)
* Online Store (think Amazon)
* Video discovery (Youtube, TikTok)
* Search (Google, Bing)

All will be completely decentralized, scalable, and cryptographically secure. We want Locutus to be useful out-of-the-box, so we plan to provide reference implementations for some or all of these.

### Architecture

A decentralized, scalable key-value store in which values are arbitrary data we call *state*, and keys are *cryptographic contracts* that control 
the creation and modification of its associated state. Contracts are implemented in [Web Assembly](https://webassembly.org/). Any participant in the network can request a contract's state, and also *subscribe* to state changes.

Locutus is implemented in Rust and will be available across all major operating systems, desktop and mobile.

### Status

We're working hard and expect an early prototype in May 2022. If you're a Rust developer and would like to help please talk to us in [#locutus](https://discord.gg/2kZuKNxYXv) on Discord. You can also support Freenet through a [donation](https://freenetproject.org/pages/donate.html).

### Name

Locutus is the development name for this software; it will probably change before launch.

### Chat with us

We're in [#locutus](https://discord.gg/2kZuKNxYXv) on Discord.

## License

This project is licensed under either of

- Apache License, Version 2.0, ([LICENSE-APACHE](LICENSE-APACHE) or
  http://www.apache.org/licenses/LICENSE-2.0)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or
  http://opensource.org/licenses/MIT)

at your option.
