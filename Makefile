.PHONY:

init:
	test -d .env || virtualenv .env
	. .env/bin/activate ; pip install -Ur requirements.txt

run:
	set -e ; . .env/bin/activate ; python3 server.py

clean:
	rm -rf .env
	find -iname "*.pyc" -delete
	find -iname "__pycache__" -delete

