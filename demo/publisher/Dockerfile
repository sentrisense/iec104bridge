FROM python:3.12-slim

WORKDIR /app
COPY requirements.txt .
RUN pip install --no-cache-dir -r requirements.txt

COPY publisher.py .

ENV NATS_URL=nats://nats1:4222 \
    NATS_SUBJECT=sensors.measurements \
    NATS_STREAM=SENSORS \
    PUBLISH_INTERVAL=2.0

CMD ["python", "publisher.py"]
