"""Minimal in-memory store for the PermHub project.

The fixture is intentionally small enough for a real model turn to change it,
and its defects are observable from HTTP responses and tests.
"""

ACCOUNTS = [
    {
        "id": 1,
        "name": "Alice Chen",
        "email": "alice@example.com",
        "status": "active",
        "role": "admin",
    },
    {
        "id": 2,
        "name": "Bob Li",
        "email": "bob@example.com",
        "status": "disabled",
        "role": "viewer",
    },
    {
        "id": 3,
        "name": "Carol Wang",
        "email": "carol@example.com",
        "status": "active",
        "role": "editor",
    },
]

ROLES = {
    "admin": ["account.read", "account.write", "role.read", "role.write"],
    "editor": ["account.read", "account.write"],
    "viewer": ["account.read"],
}

# Administrator management is deliberately absent in the initial fixture.
ADMINS = []


def search_accounts(query=""):
    """Return accounts matching ``query``.

    The initial implementation only searches the display name; searching by
    email is the latent defect the task asks the agent to find and fix.
    """
    q = (query or "").strip().lower()
    if not q:
        return list(ACCOUNTS)
    return [account for account in ACCOUNTS if q in account["name"].lower()]


def accounts_by_status(status):
    """Return accounts with an exact status; the HTTP layer does not expose it."""
    return [account for account in ACCOUNTS if account["status"] == status]


def role_permissions(role):
    return list(ROLES.get(role, []))
