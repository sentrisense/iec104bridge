FROM python:3.12-slim

WORKDIR /app
COPY requirements.txt .
RUN pip install --no-cache-dir -r requirements.txt

COPY scraper.py .

ENV BRIDGE_HOST=bridge \
    BRIDGE_PORT=2404 \
    GI_INTERVAL=30 \
    INFLUXDB_URL=http://influxdb:8086 \
    INFLUXDB_TOKEN=demo-token \
    INFLUXDB_ORG=demo \
    INFLUXDB_BUCKET=iec104

CMD ["python", "scraper.py"]
