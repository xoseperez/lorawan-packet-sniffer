// ----------------------------------------------------------------------------
// Configuration
// ----------------------------------------------------------------------------

var mode = 0; // 0 uplinks, 1 joinreq

var LORAWAN_MESSAGE_TYPES = [
    "JReq", 
    "JAcc", 
    "UnconfUp",
    "UnconfDown", 
    "ConfUp", 
    "ConfDown", 
    "RFU", 
    "Prop"
];

var CLICK_TO_FILTER_STRING = "Click to filter/unfilter";

const TIMESTRING_OPTIONS = {
    hour12: false,
    hour: '2-digit',
    minute: '2-digit',
    second: '2-digit',
};

// ----------------------------------------------------------------------------
// Methods
// ----------------------------------------------------------------------------

function fullScreen() {
    if (!document.mozFullScreen && !document.webkitFullScreen) {
        if (container.requestFullscreen) {
            container.requestFullscreen();
        }
        else if (container.mozRequestFullScreen) {
            container.mozRequestFullScreen();
        }
        else if (container.webkitRequestFullScreen) {
            container.webkitRequestFullScreen();
        }
        else if (container.msRequestFullscreen) {
            container.msRequestFullscreen();
        }
    }
}

function exitFullScreen() {
    if (document.exitFullscreen) {
        document.exitFullscreen();
    } else if (document.mozCancelFullScreen) {
        document.mozCancelFullScreen();
    } else if (document.webkitCancelFullScreen) {
        document.webkitCancelFullScreen();
    } else if (document.msExitFullscreen) {
        document.msExitFullscreen();
    }
}

function toggleFullScreen() {
    if (document.fullscreenElement) {
        exitFullScreen();
    } else {
        fullScreen();
    }
}    

function toggleScreen() {
    var xhttp = new XMLHttpRequest();
    xhttp.open("GET", "/screen", true);
    xhttp.send();
}    

function toggleMode() {
    mode = 1 - mode;
    if (mode == 0) {
        document.getElementById('uplinks_table').style.display = "block";
        document.getElementById('joinreqs_table').style.display = "none";
    } else {
        document.getElementById('uplinks_table').style.display = "none";
        document.getElementById('joinreqs_table').style.display = "block";
    }
}

function configure() {
    var xhttp = new XMLHttpRequest();
    xhttp.onreadystatechange = function() {
        if (this.readyState == 4 && this.status == 200) {
            config=JSON.parse(this.responseText);
            if (config.buttons.screen) document.getElementById('screen').style.display = "block";
            if (config.buttons.mode) document.getElementById('mode').style.display = "block";
            if (config.buttons.fullscreen) document.getElementById('fullscreen').style.display = "block";
        }
    };
    xhttp.open("GET", "/config", true);
    xhttp.send();
}    

// ----------------------------------------------------------------------------
// Tables
// ----------------------------------------------------------------------------

var uplinks_table = new Tabulator("#uplinks_table", {
    ajaxURL: "/uplinks",
    layout: "fitColumns",
    pagination: "local",
    paginationSize: 10,
    paginationCounter: "rows",
    columns:[
        {title:"TIME", field:"timestamp", sorter:"string", width: 120, hozAlign:"left", 
            formatter: function(cell, formatterParams, onRendered) {
                var date = new Date(cell.getValue() * 1000);
                return date.toLocaleTimeString([], TIMESTRING_OPTIONS);
            }
        },
        {title:"DEVADDR", field:"devaddr", sorter:"string", hozAlign:"left", headerFilter:"input", width: 120, tooltip: CLICK_TO_FILTER_STRING},
        {title:"FNCT", field:"fcnt", sorter:"number", hozAlign:"left"},
        {title:"FPORT", field:"fport", sorter:"number", hozAlign:"left", width: 100},
        {title:"SIZE", field:"size", sorter:"number", hozAlign:"left"},
        {title:"TOA", field:"toa", sorter:"number", hozAlign:"left"},
        {title:"FREQ", field:"frequency", sorter:"number", hozAlign:"left", headerFilter:"input", width: 100, tooltip: CLICK_TO_FILTER_STRING,
            formatter: function(cell, formatterParams, onRendered) {
                return cell.getValue() / 1000000;
            }
        },
        {title:"DR", field:"dr", sorter:"string", hozAlign:"left", headerFilter:"input", width: 100, tooltip: CLICK_TO_FILTER_STRING},
        {title:"RSSI", field:"rssi", sorter:"number", hozAlign:"left"},
        {title:"SNR", field:"snr", sorter:"number", hozAlign:"left", 
            formatter: function(cell, formatterParams, onRendered) {
                var snr = parseInt(cell.getValue(),10);
                var rssi = parseInt(cell.getRow().getData()['rssi'],10);
                var sf = parseInt(cell.getRow().getData()['sf'],10);
                var min_snr = 10 - 2.5 * sf;
                color = '#006100';
                if ((rssi < -115) || (snr - min_snr < 13)) color = '#0C6500';
                if ((rssi < -126) || (snr - min_snr < 5)) color = '#9C0006';
                cell.getRow().getElement().style.color = color;
                return snr;
            }
        },
        {title:"TYPE", field:"mtype", sorter:"string", hozAlign:"left", width:150, headerFilter:"input", tooltip: CLICK_TO_FILTER_STRING, 
            formatter: function(cell, formatterParams, onRendered) {
                return LORAWAN_MESSAGE_TYPES[parseInt(cell.getValue(), 10)];
            }
        },
        //{title:"ADR", field:"adr", sorter:"string", hozAlign:"left"},
        //{title:"ACK", field:"ack", sorter:"string", hozAlign:"left"},
    ],
    initialSort:[
        {column:"timestamp", dir:"desc"},
    ]    
});

uplinks_table.on("cellClick", function(e, cell) {
    if (cell.getColumn().getDefinition().headerFilter == "input") {
        var field = cell.getColumn().getField();
        var current_filter = uplinks_table.getHeaderFilterValue(field);        
        var new_filter = cell.getValue();
        if (current_filter != new_filter) {
            uplinks_table.setHeaderFilterValue(field, new_filter);
        } else {
            uplinks_table.setHeaderFilterValue(field, "");
        }
    }
});

var joinreqs_table = new Tabulator("#joinreqs_table", {
    ajaxURL: "/joinreqs",
    layout: "fitColumns",
    pagination: "local",
    paginationSize: 10,
    paginationCounter: "rows",
    columns:[
        {title:"TIME", field:"timestamp", sorter:"string", hozAlign:"left", 
            formatter: function(cell, formatterParams, onRendered) {
                var date = new Date(cell.getValue() * 1000);
                return date.toLocaleTimeString();
            }
        },
        {title:"DEVEUI", field:"deveui", sorter:"string", hozAlign:"left", headerFilter:"input", width: 200, tooltip: CLICK_TO_FILTER_STRING},
        {title:"APPEUI", field:"appeui", sorter:"string", hozAlign:"left", headerFilter:"input", width: 200, tooltip: CLICK_TO_FILTER_STRING},
        {title:"FREQ", field:"frequency", sorter:"number", hozAlign:"left", headerFilter:"input", width: 100, tooltip: CLICK_TO_FILTER_STRING,
            formatter: function(cell, formatterParams, onRendered) {
                return cell.getValue() / 1000000;
            }
        },
        {title:"DR", field:"dr", sorter:"string", hozAlign:"left", headerFilter:"input", tooltip: CLICK_TO_FILTER_STRING},
        {title:"RSSI", field:"rssi", sorter:"number", hozAlign:"left"},
        {title:"SNR", field:"snr", sorter:"number", hozAlign:"left", 
            formatter: function(cell, formatterParams, onRendered) {
                var snr = parseInt(cell.getValue(),10);
                var rssi = parseInt(cell.getRow().getData()['rssi'],10);
                var sf = parseInt(cell.getRow().getData()['sf'],10);
                var min_snr = 10 - 2.5 * sf;
                color = '#006100';
                if ((rssi < -115) || (snr - min_snr < 13)) color = '#0C6500';
                if ((rssi < -126) || (snr - min_snr < 5)) color = '#9C0006';
                cell.getRow().getElement().style.color = color;
                return snr;
            }
        },
        {title:"TYPE", field:"mtype", sorter:"string", hozAlign:"left", width:150, headerFilter:"input", tooltip: CLICK_TO_FILTER_STRING, 
            formatter: function(cell, formatterParams, onRendered) {
                return LORAWAN_MESSAGE_TYPES[parseInt(cell.getValue(), 10)];
            }
        },
    ],
    initialSort:[
        {column:"timestamp", dir:"desc"},
    ]    
});

joinreqs_table.on("cellClick", function(e, cell) {
    if (cell.getColumn().getDefinition().headerFilter == "input") {
        var field = cell.getColumn().getField();
        var current_filter = uplinks_table.getHeaderFilterValue(field);        
        var new_filter = cell.getValue();
        if (current_filter != new_filter) {
            uplinks_table.setHeaderFilterValue(field, new_filter);
        } else {
            uplinks_table.setHeaderFilterValue(field, "");
        }
    }
});

// ----------------------------------------------------------------------------
// Entry point
// ----------------------------------------------------------------------------

setInterval( function() { 
    if (0 == mode) {
        uplinks_table.replaceData(); 
    } else {
        joinreqs_table.replaceData();
    }
}, 1000 );

window.onload = function() {
    document.getElementById('fullscreen').addEventListener("click", toggleFullScreen);
    document.getElementById('mode').addEventListener("click", toggleMode);
    document.getElementById('screen').addEventListener("click", toggleScreen);
    document.getElementById('joinreqs_table').style.display = "none";
    configure();
}

