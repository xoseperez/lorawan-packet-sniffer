# --------------------------------------------------------------------
# Implementation of Paul's (@disk91) field tester server in Python3
# --------------------------------------------------------------------

import os
import sys
import re
import time
import flatdict
import datetime
import yaml
import logging
import json
import base64
import binascii
import math
import socket
import random
import threading
import flask

from flask import send_from_directory,  jsonify

from yaml.loader import SafeLoader
from paho.mqtt.client import Client

_global_uplinks = []
_global_joinreqs = []

class Config():

    _data = {}

    def __init__(self):
        try:
            with open("config.yml", "r") as f:
                self._data =  flatdict.FlatDict(yaml.load(f, Loader=SafeLoader), delimiter='.')
        except FileNotFoundError: 
            None

    def get(self, name, default=None):
        env_name = name.upper().replace('.', '_').replace('-', '_')
        value = os.environ.get(env_name)
        if value:
            if value.lower() == "true":
                return True
            if value.lower() == "false":
                return False
            return value
        return self._data.get(name, default)
  

class UDPListener():

    _socket = False

    def __init__(self, port=1700):
        self._socket = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        self._socket.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        self._socket.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEPORT, 1)
        self._socket.bind(("", port))  

    def run(self):
        while True:
            message = self._socket.recv(1024)
            yield message

class MQTTClient(Client):

    MQTTv31 = 3
    MQTTv311 = 4
    MQTTv5 = 5

    def __init__(self, broker="localhost", port=1883, username=None, password=None, userdata=None):

        def connect_callback_default(client, userdata, flags, rc):
            if rc == 0:
                logging.debug("[MQTT] Connected to MQTT Broker!")

        def subscribe_callback_default(client, userdata, mid, granted_qos):
            logging.debug("[MQTT] Subscribed")

        def disconnect_callback_default(client, userdata, rc):
            logging.debug("[MQTT] Disconnected from MQTT Broker!")

        Client.__init__(self, 
            client_id = "",
            clean_session = None,
            userdata = userdata,
            protocol = self.MQTTv311,
            transport = "tcp",
            reconnect_on_failure = True
        )

        self.on_connect = connect_callback_default
        self.on_disconnect = disconnect_callback_default
        self.on_subscribe = subscribe_callback_default
        if username and password:
            self.username_pw_set(username, password)
        self.connect(broker, port)

    def start(self):
        self.loop_start()

def process(data, config):

    global _global_uplinks, _global_joinreqs

    # https://github.com/Lora-net/packet_forwarder/blob/master/PROTOCOL.TXT

    # Check protocol version and push data identifier
    if (data[0] != 0x02) or (data[3] != 0x00):
        return False

    gateway_eui = data[4:12].hex().upper()
    message = json.loads(data[12:])
    if not 'rxpk' in message:
        return False

    phy_payload = base64.b64decode(message['rxpk'][0]['data'])
    mdhr = phy_payload[0]
    mtype = mdhr >> 5
    mac_payload = phy_payload[1:-4]

    output = {
        'gateway_eui': gateway_eui,
        'timestamp': int(datetime.datetime.now().timestamp()),
        'frequency': int(1000000 * message['rxpk'][0]['freq']),
        'dr': message['rxpk'][0]['datr'],
        'rssi': message['rxpk'][0]['rssi'],
        'snr': message['rxpk'][0]['lsnr'],
        'mtype': mtype
    }

    p = re.compile(r'SF(\d+)BW(\d+)')
    matches = p.match(output['dr'])
    if matches:
        output['sf'] = int(matches.group(1))
        output['bw'] = int(matches.group(2))

    if (mtype == 2) or (mtype == 4):
        output['devaddr'] = mac_payload[0:4][::-1].hex().upper()
        output['fcnt'] = mac_payload[5] + 256 * mac_payload[6]
        fctrl = mac_payload[4]
        output['adr'] = bool(fctrl & 0x80)
        output['adrackreq'] = bool(fctrl & 0x40)
        output['ack'] = bool(fctrl & 0x20)
        output['classb'] = bool(fctrl & 0x10)
        fopts_len = fctrl & 0x0F
        output['fport'] = mac_payload[7+fopts_len]

        _global_uplinks.append(output)
        _global_uplinks = _global_uplinks[-int(config.get('general.buffer', 500)):]

    if (mtype == 0):
        output['appeui'] = mac_payload[0:8][::-1].hex().upper()
        output['deveui'] = mac_payload[8:16][::-1].hex().upper()

        _global_joinreqs.append(output)
        _global_joinreqs = _global_joinreqs[-int(config.get('general.buffer', 500)):]

    logging.info("[DATA] %s" % json.dumps(output))
    return output

app = flask.Flask(__name__)
app.config["DEBUG"] = True

@app.route('/')
def send_home():
    return send_from_directory('web', "index.html")

@app.route('/<path:path>')
def send_page(path):
    logging.debug("[WEB] Request: %s" % path)
    return send_from_directory('web', path)

@app.route('/uplinks')
def send_uplinks():
    return jsonify(_global_uplinks)

@app.route('/joinreqs')
def send_joinreqs():
    return jsonify(_global_joinreqs)

def main():

    # load configuration file
    config = Config()

    # set logging level based on settings (10=DEBUG, 20=INFO, ...)
    level=int(config.get("logging.level", logging.DEBUG))
    logging.basicConfig(format='[%(asctime)s] %(message)s', level=level)
    logging.debug("[MAIN] Setting logging level to %d" % level)

    # configure MQTT connection
    mqtt_enabled = config.get('mqtt.enabled', False)
    if mqtt_enabled:
        mqtt_topic = config.get('mqtt.topic', 'gts/message')
        mqtt_client = MQTTClient(
            config.get('mqtt.server', 'locahost'), 
            int(config.get('mqtt.port', 1883)),
            config.get('mqtt.username'),
            config.get('mqtt.password')
        )
        mqtt_client.start()

    # configure web
    web_enabled = config.get('web.enabled', True)
    if web_enabled:
        threading.Thread(target=lambda: app.run(host="0.0.0.0", port=int(config.get('web.port', 8888)), debug=True, use_reloader=False)).start()

    # configure UDP listener
    listener = UDPListener(int(config.get('udp.port', 1700)))
    for data in listener.run():
        message = process(data, config)
        if message:
            if mqtt_enabled:
                mqtt_client.publish(mqtt_topic, json.dumps(message))

if (__name__ == '__main__'): 
    main()