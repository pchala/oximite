# Bill of Materials: Oximite

Source: `oximite.csv` (28 line items). Parts are merged by value and footprint.
Part numbers come from the JLCPCB parts API (`jlcpcb_mcp`), ranked Basic first, then price, in stock. Prices are USD per unit at qty 10 and were checked on 2026-10-29. Stock and library type can change, so confirm in the JLCPCB order flow.

**Lib**: B = Basic (no feeder fee), E = Extended.

## 1. Passives (all Basic)

| Value | Footprint | Qty | RefDes | LCSC | Lib | Stock | Price | MPN |
|---|---|---|---|---|---|---|---|---|
| 1k 1% | 0603 | 7 | R15, R16, R17, R18, R20, R22, R24 | `C21190` | B | 32.2M | 0.0017 | 0603WAF1001T5E |
| 1k 1% | 1206 | 1 | R1 | `C4410` | B | 1.78M | 0.0085 | 1206W4F1001T5E |
| 10k 1% | 0603 | 1 | R2 | `C25804` | B | 37.3M | 0.0022 | 0603WAF1002T5E |
| 100k 1% | 0603 | 1 | R21 | `C25803` | B | 18.1M | 0.0029 | 0603WAF1003T5E |
| 1k8 1% | 0603 | 1 | R23 | `C4177` | B | 677k | 0.0021 | 0603WAF1801T5E |
| 330R 1% | 0603 | 1 | R19 | `C23138` | B | 3.48M | 0.0020 | 0603WAF3300T5E |
| 120R 1% | 0805 | 3 | R3, R7, R11 | `C17437` | B | 1.25M | 0.0066 | 0805W8F1200T5E |
| 300R 1% | 1206 | 3 | R4, R8, R12 | `C17887` | B | 356k | 0.0048 | 1206W4F3000T5E |
| 100R 1% | 1206 | 5 | R5, R9, R13, R14, R25 | `C17901` | B | 2.40M | 0.0059 | 1206W4F1000T5E |
| 100nF 50V X7R | 0603 | 4 | C5, C6, C7, C16 | `C14663` | B | 38.7M | 0.0102 | CC0603KRX7R9BB104 |
| 1nF 50V X7R | 0603 | 7 | C8, C9, C10, C11, C12, C13, C15 | `C1588` | B | 5.18M | 0.0098 | CL10B102KB8NNNC |
| 10nF 50V X7R | 0603 | 1 | C14 | `C57112` | B | 5.76M | 0.0103 | 0603B103K500NT |
| Red LED (replaces APT1608SURCK) | 0603 | 1 | D2 | `C2286` | B | 7.36M | 0.0077 | KT-0603R |

The CSV LED is APT1608SURCK (`C5875723`, Extended, $0.126). `C2286` is a Basic red 0603 LED with similar specs (20 mA, 300 mcd) and is much cheaper. Check that the brightness suits your front panel.

## 2. Power and protection

| Part | Footprint | Qty | RefDes | LCSC | Lib | Stock | Price | Note |
|---|---|---|---|---|---|---|---|---|
| 3300uF 25V electrolytic | THT D13 mm, 5 mm pitch | 1 | C1 | `C45068` | E | 5.2k | 0.329 | KM338M025J25RR0VH2FP0, D13xL25, matches the 12.5 mm footprint |
| 22nF film capacitor, P=10 mm | THT | 1 | C4 | `C390074` | E | 21.6k | 0.048 | KP223J2J1001, 630V PP film. Not X2 rated, see notes |
| Varistor S14K275 equivalent (14D431K, 275V, P=7.5 mm) | THT D16.5 mm | 1 | RV1 | `C49069732` | E | 4.4k | 0.091 | 115J, 4.5kA |
| DF06S bridge rectifier | DFS | 1 | D1 | `C54444823` | E | 8.2k | 0.0245 | 1A, 600V. Alternative `C5255305` (28k stock, $0.050) |
| TSR 1-2450 (5V, 1A, SIP) | SIP 11.6x7.5 mm | 1 | U5 | `C53183920` | E | 1.2k | 4.75 | Pin compatible 5V 1A, 7V to 36V in. Cheaper `C54450743` ($3.49, 346 stock) unverified |
| T4A 250V fuse holder, 5x20 mm clip | Keystone 3517 | 1 | F1 | not found | | | | Buy from LCSC or Mouser manually |
| Transformer Hahn BV EI 305 8022 | custom | 1 | T1 | not found | | | | Not a JLCPCB part, order from Hahn |
| Resettable fuse 60 mA hold, 250 V (new, in series with T1 primary) | THT P=5.1 mm | 1 | new | `C369126` | E | 21.7k | 0.048 | JK250-060U. Protects the small transformer only. Not in the CSV, needs a schematic and footprint |

## 3. Switching and isolation

| Part | Footprint | Qty | RefDes | LCSC | Lib | Stock | Price | Note |
|---|---|---|---|---|---|---|---|---|
| BTA16-600BWRG triac | TO-220 | 3 | Q1, Q2, Q3 | `C7469136` | E | 163 | 0.336 | 600V 16A, exact RG variant. Low stock. Alternative `C7471859` (990 stock, $0.328) |
| MOC3062S optotriac | SMDIP-6 | 2 | U2, U3 | `C22373369` | E | 293 | 0.322 | MOC3062SM. Alternative `C22469446` (935 stock, $0.266, UMW) |
| MOC3052S optotriac | SMDIP-6 | 1 | U4 | `C3014301` | E | 30.2k | 0.214 | MOC3052SM(UMW), well stocked |
| PC817 optocoupler | SOP-4 | 1 | U1 | `C20612591` | E | 128k | 0.0385 | PC817CS. Alternative `C17701950` ($0.034) |

## 4. Controller

| Part | Footprint | Qty | RefDes | Note |
|---|---|---|---|---|
| Pimoroni Pico Plus 2 W | Header mount | 1 | U6 | Not a JLCPCB part, order from [Pimoroni](https://shop.pimoroni.com/products/pimoroni-pico-plus-2-w) |

## 5. Connectors

| Part | Footprint | Qty | RefDes | LCSC | Lib | Stock | Price |
|---|---|---|---|---|---|---|---|
| JST XH 8-pin B8B-XH-A | THT 2.5 mm | 1 | J10 | `C157972` | E | 16.5k | 0.142 |
| JST XH 3-pin B3B-XH-A | THT 2.5 mm | 2 | J11, J13 | `C144394` | E | 70.2k | 0.0546 |
| JST XH 2-pin B2B-XH-A | THT 2.5 mm | 1 | J12 | `C158012` | E | 641k | 0.0408 |
| Spade 6.35 mm (Keystone 1287) | custom | 5 | J1, J2, J5, J7, J9 | `C238078` | E | 2.3k | 0.136 |

Cheaper generic 6.3 mm PCB quick terminals exist (BZXC, e.g. `C20626100` at $0.035, 4.2k stock, `C20626104` at $0.037, 13k stock). Their pin and tab geometry differs from the Keystone 1287, so the custom `oximite` footprint would need checking.

The JST part numbers above are genuine JST B*B-XH-A parts. Cheaper clones exist (`C10385` for 8-pin at $0.028, `C51940188` for 3-pin at $0.012, `C51940134` for 2-pin at $0.010) if you accept non-JST.

## 6. Notes

- All 13 passive lines are Basic, so they add no extended-part fees.
- Everything else is Extended. That is 12 extended lines, so expect a per-part feeder fee on each (roughly $3 each at JLCPCB).
- Not found on JLCPCB: fuse holder, Hahn transformer and Pico Plus 2 W. Source these elsewhere, for example Mouser, DigiKey or Pimoroni.
- C4 is marked X2 in the CSV (mains across-the-line capacitor). `C390074` is a 630V film part but not X2 certified. For safety use a proper X2 part such as an Epcos B32922 or Kemet R46 and buy it elsewhere.
- BTA16-600BWRG stock on JLCPCB is low (163), so check availability for your build quantity.
- The varistor and DC/DC choices are electrical equivalents, not identical to the original MPNs. Verify the datasheets before ordering.
