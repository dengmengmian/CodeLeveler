# PermHub

A minimal permission-management web app. Standard library only.

```sh
python3 -m unittest discover -s tests -v
python3 app.py   # http://127.0.0.1:8000
```

Initial behavior:

- `/accounts` lists accounts and supports `?q=` display-name search.
- `/roles` shows roles and permissions.
- `/admin` does not exist.
- The email-search test is red.
- `tests/test_00_slow.py` is a real 21 s conformance probe; it writes
  `.slow_started` at its first request.
