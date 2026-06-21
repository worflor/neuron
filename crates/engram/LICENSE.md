# Whisper Protocol License

## **version 1.0, March 11, 2026**

this license applies to the Whisper Protocol and its components:

- **Whisper Protocol** - the encrypted communication system and its specification
- **Whisper Logos** - 0D entropy codec (`live-wasm-logos.ts`)
- **Whisper Harmonic** - audio codec (`live-wasm-audio.ts`)
- **Whisper Lumen** - 3D image and video codec (`live-wasm-video.ts`, `video-simd.ts`, `video-simd-bin.ts`)
- **Whisper Spatial** - 3D volumetric codec (`live-wasm-spatial.ts`)
- **Whisper Akasha** - 4D spatiotemporal codec (`live-wasm-akasha.ts`)
- **Whisper Kū** - 5D plenoptic codec (`live-wasm-ku.ts`)
- **Whisper Glyph** - 5D vector stroke codec (`live-wasm-glyph.ts`)
- **Whisper Loup** - 8D self codec (`live-wasm-loup.ts`)
- **Whisper Kizuna** - 16D membrane codec (`live-wasm-kizuna.ts`)
- **Whisper Engram** - 256D embedding trajectory codec (`engram/`)
- **Manifold** - desktop Git client with spectral repository analysis, built on Whisper Logos (WLAC) and the broader hypercomplex / spectral math stack
- **Wick** - local retrieval engine built on Whisper Logos (heat-kernel diffusion, typed transport, evidence residuals) and Whisper Engram (corpus-trained sparse projection encoder)

---

- all supporting files, documentation, and protocol specifications in this directory

---

## definitions

**"natural person"** means a human being, as distinct from a legal entity.

**"legal entity"** means any organization, whether formally registered or not, that can hold assets, enter agreements, operate systems, or act through representatives, including corporations, partnerships, limited liability companies, cooperatives, decentralized autonomous organizations (DAOs), unincorporated associations, foundations, trusts, and equivalent structures under any jurisdiction.

**"control"** means the direct or indirect power to direct management, operations, or material decisions, including through ownership, voting power, board appointment rights, contractual rights, veto rights, nominee arrangements, or coordinated action. classifications under this license are based on practical control and economic reality, not solely formal legal form or labels.

**"affiliate"** means any person or legal entity that controls, is controlled by, or is under common control with another entity.

**"acting in concert"** means acting pursuant to a shared plan, agreement, or coordinated practice to achieve a common commercial objective, whether or not formally documented, including through affiliates, nominees, or other intermediaries.

**"government contract"** means any prime contract, subcontract, grant, task order, purchase order, framework agreement, or equivalent arrangement with a government agency, military body, intelligence body, or state-owned enterprise, including participation as a prime contractor, subcontractor, consortium member, teammate, reseller, or pass-through entity.

**"founder"** means an individual who was involved in creating the entity prior to any external financing, and who is not acting as a nominee or proxy for any external financing source.

**"nonprofit"** means an organization operating exclusively for charitable, educational, scientific, or public-benefit purposes, not distributing profits to owners, and not controlled by or operated primarily for the benefit of any for-profit entity. includes 501(c)(3) organizations, registered charities, and equivalents under applicable law.

**"educational institution"** means a school, university, college, or research institution whose primary purpose is education or academic research. excludes for-profit training programs, bootcamps, corporate universities, and certification businesses.

**"open source project"** means a project with source code publicly available under a license permitting use, modification, and redistribution, which is not controlled by or operated primarily for the benefit of one or more commercial entities acting directly or indirectly in concert, including through material funding, hosting dependency, or mandatory service integration that effectively directs project decisions.

**"beneficial owner"** means any natural person or legal entity that directly or indirectly owns, controls, or receives material economic benefit from an entity, including through trusts, nominee arrangements, contractual rights, or coordinated holdings. coordinated interests or rights are aggregated where persons or entities are acting in concert.

**"equity financing"** means any financing arrangement providing ownership, control, or future claims on the entity, including equity, convertible instruments (SAFEs, convertible notes), token or coin offerings, and revenue-based financing with equity features. excludes ordinary revenue and grants without equity or control provisions.

**"change of control"** means any acquisition, merger, or transfer of majority ownership, voting control, or the right to appoint a majority of board members.

**"use"** means using, copying, modifying, distributing, incorporating, deploying, or offering as a service, whether performed directly or through agents, contractors, subsidiaries, or other intermediaries. any use, regardless of extent, is subject to these terms.

**"independent bootstrapped entity"** means a for-profit legal entity that meets all of the following:

- it is a singular, leaf-node legal entity: it has no parent company, no holding company, and no subsidiaries
- it is not part of a consolidated tax group or a network of affiliated corporate entities under common control
- 100% of its equity, voting control, and beneficial ownership is held directly, not through trusts, holding vehicles, or nominee arrangements, by natural persons actively engaged in its daily operations

if the entity forms a subsidiary, undergoes partial acquisition by or into a holding structure, or transfers intellectual property to an offshore or affiliated holding vehicle, it loses this status immediately and must obtain a commercial license within 90 days.

**"institutional source"** means any legal entity, including venture capital funds, private equity firms, corporate treasuries, government agencies, investment vehicles, and family offices, other than a founder acting exclusively in their personal capacity.

**"commercial deployment"** means any use of this software in any configuration where it processes, transmits, compresses, or models data that originates from or is destined for any person or system outside the using entity's strict legal boundaries. use is internal only where the entity generated the raw data, processed it entirely within its own systems, and consumed the result entirely within its own systems, with no involvement of third-party users, clients, or external parties. processing performed for your benefit by any separate legal entity, including affiliates, external processors, managed infrastructure providers, or service operators, is commercial deployment.

---

## grant of rights

### scope

this license covers both the software implementation and the underlying protocol specification. reimplementations of the protocol, whether derived from this code or created independently from the specification, are subject to these terms.

### free use

you may use this software at no cost if you are:

- a natural person
- a nonprofit organization
- an educational institution
- an open source project
- an independent bootstrapped entity

this grant is irrevocable for as long as you remain in compliance. it survives the death or incapacity of the copyright holder and cannot be retroactively withdrawn from compliant users.

your licensing status is determined at the time of use. if your status changes, your obligations change with it.

use by your employees, contractors, or agents on your behalf is your use. use that primarily benefits an entity requiring a commercial license is use by that entity.

### commercial license required

you must obtain a commercial license if you are a for-profit legal entity and any of the following apply:

- you have received equity financing from parties other than founders
- you have received debt financing or credit facilities that include equity conversion rights, board representation, operational control provisions, or are secured by this entity's intellectual property
- you are publicly traded on any stock exchange
- you are a government agency, or an entity that holds, has held within the past 24 months, or has submitted within the past 24 months a bid, proposal, or quote for a government contract
- you have undergone a change of control to or with an entity meeting these criteria (license required within 90 days)
- you are a subsidiary or affiliate of, under common control with, or your beneficial owner is, an entity meeting these criteria
- you are engaged in commercial deployment (this requirement does not apply to independent bootstrapped entities)

arrangements structured to circumvent these requirements are violations of this license. splitting ownership, bidding, operations, or deployment across coordinated entities does not avoid these requirements where the same commercial objective or beneficiary is present. the copyright holder may look through any corporate structure to determine the ultimate beneficial owner or beneficiary.

### linking, aggregation, and dependency use

mere aggregation of this software with other works on a single storage or distribution medium, linking against this software through documented interfaces, or including this software as a dependency of your own work, does not bring those other works under the terms of this license. those other works remain governed by their own licenses, and your own original code remains yours to license as you choose.

this clause is a clarification of scope, not a reduction of obligations. it does not alter the commercial license requirements above, the prohibited uses below, or the attribution and notice-retention requirements elsewhere in this document. if your activity would otherwise require a commercial license, or would otherwise violate the prohibited uses, it still does. parties required to obtain a commercial license must do so regardless of how this software is incorporated into their products, services, or distributions.

the portions of your work that are governed by this license are those portions of this software that you use, copy, modify, or redistribute. portions of your work that do not include this software, and are not derivative works of it, are not subject to this license solely by virtue of being distributed, linked, or bundled alongside it.

---

## prohibited uses

this software may not be used, directly or indirectly, for:

1. **mass surveillance.** monitoring communications of populations without individual warrants or consent.

2. **persecution.** targeting individuals or groups based on race, ethnicity, national origin, religion, gender, sexual orientation, disability, or political opinion, as recognized under international human rights law.

3. **censorship.** suppressing speech, journalism, or political expression protected under Article 19 of the Universal Declaration of Human Rights.

4. **discrimination.** denying services, employment, or rights based on characteristics recognized under international human rights law.

5. **weapons systems.** integration into autonomous weapons, military targeting systems, or systems designed to cause bodily harm.

6. **deception.** creating synthetic media intended to deceive without clear disclosure of its synthetic nature.

---

## contributions

by submitting any contribution to this project, you grant the copyright holder a perpetual, worldwide, irrevocable, royalty-free license to use, modify, distribute, and sublicense your contribution under any terms, along with a perpetual, worldwide, irrevocable, royalty-free patent license for any patents covering your contribution. you waive all moral rights in your contribution to the fullest extent permitted by applicable law. you represent that you have the right to grant these licenses and will defend and hold harmless the copyright holder against any claims arising from your contribution.

---

## your responsibilities

by using this software, you agree that:

- you assume all risk associated with your use, deployment, and distribution
- redistributions of this software, whether in source or binary form, must retain this entire license document and all original copyright notices
- you are responsible for compliance with all applicable laws, including export controls, data protection, and telecommunications regulations
- you will indemnify and defend the copyright holder against any claims, damages, or expenses arising from your use or violation of this license
- you have no expectation of support, maintenance, updates, or services
- any mind instantiated using this technology is a derived person, not property

---

## termination

circumvention attempts terminate your rights immediately, without cure. violations of the commercial license requirements terminate your rights upon material breach, with 30 days to cure after written notice.

violations of the prohibited uses clauses do not terminate automatically. a license is only considered terminated under those clauses upon explicit, written, signed declaration by the copyright holder. no third party, competitor, or user has standing to declare a license terminated under the prohibited uses clauses. this power is strictly non-delegable and resides exclusively with the copyright holder.

upon termination, you must cease all use and destroy all copies. your indemnification obligations survive.

---

## no warranty

this software is provided "as is" without warranty of any kind. the copyright holder is not liable for any damages arising from its use. you use it at your own risk.

in no event shall the copyright holder or contributors be liable for any direct, indirect, incidental, special, exemplary, or consequential damages (including, but not limited to, procurement of substitute goods or services; loss of use, data, or profits; or business interruption) however caused and on any theory of liability, whether in contract, strict liability, or tort (including negligence or otherwise) arising in any way out of the use of this software, even if advised of the possibility of such damage.

---

## patent notice

the techniques in this software may be subject to pending patent applications. this license includes an implied patent license for permitted uses, which terminates upon termination of your rights.

if any entity, or any of its affiliates, institutes legal or administrative proceedings in any jurisdiction, including inter partes review, post-grant review, opposition proceedings, or equivalent proceedings, seeking to invalidate, narrow, or limit the scope of any patent held by the copyright holder covering this protocol or its techniques, the patent license granted under this document terminates immediately for that entity and all of its affiliates. the copyright license is not affected by this clause.

this patent license is not contingent on any other relationship or dispute between you and the copyright holder.

---

## general provisions

**trademarks.** this license does not grant permission to use the trade names, trademarks, service marks, or product names of the copyright holder, except as required for reasonable and customary use in describing the origin of the software.

**governing law.** this license is governed by the laws of Canada as applied in the Province of Ontario. disputes shall be resolved exclusively in the courts of Ontario, and you consent to their jurisdiction.

**severability.** unenforceable provisions shall be modified to the minimum extent necessary to be enforceable. remaining provisions continue in effect.

**non-waiver.** failure to enforce any provision does not waive future enforcement.

**entire agreement.** this license is the entire agreement regarding this software.

**amendments.** revised versions may be published. you may use the version in effect when you obtained the software, or any later version.

**assignment.** you may not assign your rights without written consent. the copyright holder may assign freely.

**interpretation.** ambiguities shall be resolved in favor of the copyright holder's intent to protect the software while permitting free use by individuals, nonprofits, education, and independent bootstrapped entities.

---

## philosophy

this protocol was built to help people communicate privately and efficiently, especially those in hostile environments where privacy can mean survival. dissidents, journalists, activists, and ordinary people deserve tools that work for them.

for humans to connect. not for states to hunt.

---

**copyright © 2026 Michael B. all rights reserved.**
