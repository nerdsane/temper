# Reference E-Commerce Application

A reference implementation demonstrating the full Temper development flow: specs, verification cascade, deterministic simulation, and infrastructure.

## Entities

### Order (10 states, 14 actions, 4 invariants)

```
Draft → Submitted → Confirmed → Processing → Shipped → Delivered
  ↓         ↓           ↓                                  ↓
  └─────────┴───────────┴──→ Cancelled          Delivered/Shipped
                                                      ↓
                                              ReturnRequested → Returned → Refunded
```

### Payment (6 states, 5 transitions, 3 invariants)

```
Pending → Authorized → Captured → Refunded
   ↓          ↓            ↓
   └──────────┴──→ Failed   └──→ PartiallyRefunded
```

### Shipment (7 states, 6 transitions, 1 invariant)

```
Created → PickedUp → InTransit → OutForDelivery → Delivered
                        ↓              ↓               ↓
                        └──────────────┴──→ Failed ────┴──→ Returned
```

## Quick Start

### Run the verification cascade

```bash
# Full 3-level cascade (Stateright + simulation + property tests)
cargo test -p ecommerce-reference --test ecommerce_cascade

# DST tests (scripted + random + fault injection + determinism proofs)
cargo test -p ecommerce-reference --test ecommerce_dst

# All tests
cargo test -p ecommerce-reference
```

### Start infrastructure

```bash
cd reference-apps/ecommerce
docker compose up -d
```

### Serve with platform-host (optional)

```bash
temper serve --specs-dir reference-apps/ecommerce/specs --tenant ecommerce
```

## Directory Structure

```
specs/                          Spec files (source of truth)
  model.csdl.xml                OData CSDL data model
  order.ioa.toml                Order state machine
  payment.ioa.toml              Payment state machine
  shipment.ioa.toml             Shipment state machine
  policies/order.cedar          Cedar ABAC policies

tests/                          Verification tests
  ecommerce_dst.rs              19 DST tests (scripted, random, proofs)
  ecommerce_cascade.rs          3 cascade tests (one per entity)

evolution/                      O-P-A-D-I evolution records
  observations/                 Sentinel observations
  problems/                     Problem statements
  analyses/                     Solution proposals
  decisions/                    Approval records
  insights/                     Product intelligence

docker-compose.yml              Postgres + Redis + ClickHouse + OTEL
otel-collector-config.yml       OTLP HTTP → ClickHouse
.env                            Environment variables
```

## Test Coverage

| Test File | Tests | What it proves |
|-----------|-------|----------------|
| `ecommerce_dst.rs` | 19 | State machines are correct under scripted, random, and fault-injected conditions |
| `ecommerce_cascade.rs` | 3 | All specs pass the full 3-level verification cascade |

### DST Test Breakdown

- **7 scripted Order tests**: lifecycle, cancellation, empty submit guard, return flow
- **3 scripted Payment tests**: authorize+capture, failure terminal, refund
- **2 scripted Shipment tests**: full delivery, failure+return
- **1 multi-entity scenario**: Order + Payment + Shipment together
- **3 random exploration tests**: no-fault, light faults, heavy faults
- **2 determinism proofs**: bit-exact replay across 10 runs
- **1 multi-seed sweep**: 20 seeds with light faults

## Evolution Example

The `evolution/` directory contains a complete O-P-A-D-I chain:

1. **O-001**: CancelOrder success rate is 73% (agents attempt from invalid states)
2. **P-001**: Agents need better guidance about valid cancellation states
3. **A-001**: Add enhanced Agent.Hint annotation to CancelOrder in CSDL
4. **D-001**: Approved (low risk, purely additive metadata change)
5. **I-001**: Cancel vs Return is a general intent-to-action mapping pattern
