FROM python:3.12-alpine

WORKDIR /app
ADD requirements.txt  ./
RUN pip install -r requirements.txt
ADD server.py ./
RUN mkdir web
ADD web/* ./web/

CMD ["python", "server.py"]
