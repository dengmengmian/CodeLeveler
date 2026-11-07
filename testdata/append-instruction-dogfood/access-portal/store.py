"""Minimal in-memory store for the AccessPortal fixture.

The project is intentionally small enough for one real model turn to change it,
and everything the task asks for is observable from HTTP responses.
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


def admins():
    """Administrators are the accounts whose role is ``admin``."""
    return [account for account in ACCOUNTS if account["role"] == "admin"]


def role_permissions(role):
    return list(ROLES.get(role, []))
