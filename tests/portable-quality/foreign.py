# Synthetic scanner input: never execute.
import ssl
import subprocess

# ruleid: python-no-shell-execution
subprocess.run("printf synthetic", shell=True)
# ok: python-no-shell-execution
subprocess.run(["printf", "synthetic"], check=True)

# ruleid: python-no-disabled-tls-verification
context = ssl._create_unverified_context()
# ok: python-no-disabled-tls-verification
context = ssl.create_default_context()

# ruleid: python-review-formatted-sql
database.execute(f"SELECT * FROM {table}")
# ok: python-review-formatted-sql
database.execute("SELECT * FROM entries WHERE id = ?", (identifier,))
