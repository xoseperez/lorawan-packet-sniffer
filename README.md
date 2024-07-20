LoRaWAN Packet Sniffer
=============================

The LoRaWAN Packet Sniffer service connects to a UDP Packet Forwarder to extract and report basic metrics for each received message. It does not require a LoRaWAN nextwork server and, therefore, can monitor any LoRaWAN device that transmit within the range of the gateway.

This folder contains a python script that receives the data from the packet forwarder and reports it using a websocket.

## Configuration

The script gets the configuration information from a `config.yml` file in the same folder. This configuration file has the following options:

* logging section
  * level: logging level (10:debug, 20:info, 30:warning, 40:error)
* udp:
  * port: port to listen to UDP Packet Forwarder packets (defalts to 1700)
* mqtt:
  * enabled: enable MQTT output (defaults to False)
  * server: MQTT broker to connect to (defaults to localhost)
  * port: MQTT port (defaults to 1883)
  * username: MQTT username
  * password: MQTT password
  * topic: MQTT topic to publish messages to (defaults to gts/example)
* web:
  * enabled: enable WEB interface (defaults to True)
  * port: HTTP port (defaults to 8888)
* general:
  * buffer: store these many messages in buffer (defaults to 500)


## Usage

### Python virtual environment 

The recommended way to run the script is by using a virtual environment to install the dependencies in `requirements.txt`. We have provided a custom `Makefile` to help you run it in an isolated python environment by:

```
make init
make run
```

You only need to run the `make init` the first time.

### Docker

You can also use docker to run the service isolated from your system. We have provided custom `Dockerfile` and `docker-compose.yml` files for this. Please check the `docker-compose.yml` file for an example on the different ways to configure the service (mount a `config.yml` file or use environment variables).

```
docker compose build
docker compose up
```

## Contribute

There are several ways to contribute to this project. You can [report](http://github.com/xoseperez/lorawan-packet-sniffer/issues) bugs or [ask](http://github.com/xoseperez/lorawan-packet-sniffer/issues) for new features directly on GitHub.
You can also submit your own new features of bug fixes via a [pull request](http://github.com/xoseperez/lorawan-packet-sniffer/pr).

## License

This project is licensed under [Apache 2.0](http://www.apache.org/licenses/LICENSE-2.0) license.
