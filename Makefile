.PHONY:

init:
	test -d .venv || virtualenv .venv
	. .venv/bin/activate ; pip install -Ur requirements.txt

run:
	set -e ; . .venv/bin/activate ; python3 server.py

clean:
	rm -rf .venv
	find -iname "*.pyc" -delete
	find -iname "__pycache__" -delete

