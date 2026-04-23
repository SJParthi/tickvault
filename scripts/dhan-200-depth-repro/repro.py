"""
Dhan 200-level depth reproducer — OFFICIAL DHAN PYTHON SDK.

Baseline proof that Dhan's 200-level depth WebSocket works correctly for
our account when accessed via their official SDK. Used to confirm that
the disconnect issue seen in our Rust client is NOT a Dhan-side problem.

Usage:
    pip install dhanhq==2.2.0rc1    # requires Python 3.10+
    export DHAN_CLIENT_ID='1106656882'
    export DHAN_ACCESS_TOKEN='<fresh JWT from web.dhan.co>'
    python repro.py

First run on 2026-04-23 against SecurityId 72271 (NSE_FNO, ~ATM-182 option)
streamed 200-level depth continuously for 30+ minutes. Zero disconnects.
Same account, same token, same SecurityId that our Rust client
TCP-reset on repeatedly — proving the bug is in our Rust WebSocket
client, not Dhan.

The SDK URL logged by this script is:
    wss://full-depth-api.dhan.co/?token=<JWT>&clientId=<ID>&authType=2

Note the ROOT path '/' — not '/twohundreddepth' as we originally
documented in .claude/rules/dhan/full-market-depth.md rule 2 based on
Dhan ticket #5519522. That rule needs revision.

See .claude/plans/active-plan.md for the full Phase 1 → 3 diagnosis plan.
"""

import os
import sys

from dhanhq import DhanContext, FullDepth


def main() -> int:
    client_id = os.environ.get("DHAN_CLIENT_ID")
    access_token = os.environ.get("DHAN_ACCESS_TOKEN")

    if not client_id or not access_token:
        print(
            "ERROR: DHAN_CLIENT_ID and DHAN_ACCESS_TOKEN must be set in env.",
            file=sys.stderr,
        )
        print(
            "  export DHAN_CLIENT_ID='1106656882'\n"
            "  export DHAN_ACCESS_TOKEN='<fresh JWT from web.dhan.co>'",
            file=sys.stderr,
        )
        return 1

    dhan_context = DhanContext(client_id, access_token)

    # (2, "72271") = NSE_FNO segment, SecurityId 72271 (NIFTY option, ~ATM-182)
    # For 200-level depth: exactly ONE instrument per connection.
    # Change this SecurityId to match current ATM strike on retest days.
    instruments = [(2, "72271")]
    depth_level = 200

    try:
        response = FullDepth(dhan_context, instruments, depth_level)
        response.run_forever()

        while True:
            response.get_data()
            if response.on_close:
                print("Server disconnection detected. Kindly try again.")
                break
    except Exception as e:  # pylint: disable=broad-except
        print(f"Exception: {e}", file=sys.stderr)
        return 2

    return 0


if __name__ == "__main__":
    sys.exit(main())
