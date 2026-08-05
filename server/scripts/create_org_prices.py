#!/usr/bin/env python3
"""Create the Business/Enterprise Products + recurring Prices in USD, then print the
four ready-to-paste ORG_PRICE_* lines (with the real price IDs).

Sibling of create_stripe_prices.py, which does the same for the one-time consumer
credit packages. Same rule: the key is read from STRIPE_SECRET_KEY and never
printed. Run it with your key inline so the key stays on your machine:

    STRIPE_SECRET_KEY=sk_live_xxx python3 server/scripts/create_org_prices.py

Use the SAME mode (test vs live) as the key the server runs with — Stripe prices
are not shared between test and live.

WHY THIS EXISTS
---------------
The org plans were created in EUR while everything else bills in USD
(`stripe_handler.rs` opens Checkout with `currency: "usd"`, and the credit unit is
100 credits = $1). The marketing site and dashboard therefore quoted EUR 49/199
against a USD product. The owner chose to move the plans to USD at the same face
numbers.

NOTE THIS IS A PRICE CUT, NOT A CONVERSION. EUR 49 is roughly USD 53 at recent
rates, so USD 49 collects less per seat-month than the EUR price did. That was a
deliberate commercial decision, recorded here so nobody later reads it as a bug.

A Stripe Price is immutable — you cannot change the currency of an existing one.
This creates NEW prices; the old EUR ones stay live until you swap the env vars,
and existing subscriptions keep billing on the price they were created with. Move
current subscribers deliberately, not by assuming this script did it.

AFTER RUNNING
-------------
1. Paste the four printed values into Railway (server service, production AND
   staging) replacing the existing ORG_PRICE_* values.
2. Only THEN ship the copy change from EUR to USD on the website and dashboard.
   Shipping the copy first advertises a price Stripe is not charging.
3. Archive the old EUR prices in the Stripe dashboard once no subscription
   references them.
"""
import json
import os
import sys
import urllib.error
import urllib.parse
import urllib.request

# env var suffix, product name, interval, amount the customer pays (USD).
# Annual is 10x monthly — the "2 months free" the pricing page advertises.
PLANS = [
    {"var": "ORG_PRICE_BUSINESS_MONTHLY", "product": "VoxTranslate Business",
     "interval": "month", "price_usd": 49.00},
    {"var": "ORG_PRICE_BUSINESS_ANNUAL", "product": "VoxTranslate Business",
     "interval": "year", "price_usd": 490.00},
    {"var": "ORG_PRICE_ENTERPRISE_MONTHLY", "product": "VoxTranslate Enterprise",
     "interval": "month", "price_usd": 199.00},
    {"var": "ORG_PRICE_ENTERPRISE_ANNUAL", "product": "VoxTranslate Enterprise",
     "interval": "year", "price_usd": 1990.00},
]

sk = os.environ.get("STRIPE_SECRET_KEY", "")
if not sk.startswith("sk_"):
    sys.exit("Set STRIPE_SECRET_KEY to a secret key (sk_test_… or sk_live_…).")
mode = "LIVE" if sk.startswith("sk_live_") else "TEST"
print(f"# Creating org subscription prices in Stripe {mode} mode (USD)\n", file=sys.stderr)


def stripe_post(path, data):
    body = urllib.parse.urlencode(data).encode()
    req = urllib.request.Request(
        "https://api.stripe.com/v1/" + path, data=body,
        headers={"Authorization": "Bearer " + sk},
    )
    try:
        with urllib.request.urlopen(req, timeout=30) as r:
            return json.loads(r.read())
    except urllib.error.HTTPError as e:
        sys.exit(f"Stripe error on {path}: {e.code} {e.read().decode()[:300]}")


# One Product per plan, reused across its monthly and annual Price — that is how
# Stripe models a plan with two billing intervals, and it keeps the Billing Portal
# able to offer an interval switch rather than a plan change.
products = {}
for plan in PLANS:
    name = plan["product"]
    if name not in products:
        products[name] = stripe_post("products", {"name": name})["id"]
    cents = int(round(float(plan["price_usd"]) * 100))
    price = stripe_post("prices", {
        "product": products[name],
        "unit_amount": cents,
        "currency": "usd",
        "recurring[interval]": plan["interval"],
    })
    plan["price_id"] = price["id"]
    print(f"  {plan['var']:32} ${plan['price_usd']:<8} /{plan['interval']:5} -> {price['id']}",
          file=sys.stderr)

print("\n# Paste these into Railway (server service) for BOTH production and staging,\n"
      "# replacing the existing EUR price ids. Values only, no quotes needed:\n",
      file=sys.stderr)
for plan in PLANS:
    print(f"{plan['var']}={plan['price_id']}")
