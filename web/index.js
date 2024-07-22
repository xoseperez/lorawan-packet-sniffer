var LORAWAN_MESSAGE_TYPES = [
    "Join Request", 
    "Join Accept", 
    "Unconfirmed Data Up",
    "Unconfirmed Data Down", 
    "Confirmed Data Up", 
    "Confirmed Data Down", 
    "RFU", 
    "Propietary"
];

var CLICK_TO_FILTER_STRING = "Click to filter/unfilter";

var uplinks_table = new Tabulator("#uplinks-table", {
    ajaxURL: "/uplinks",
    layout: "fitColumns",
    columns:[
        {title:"TIME", field:"timestamp", sorter:"string", hozAlign:"left", 
            formatter: function(cell, formatterParams, onRendered) {
                var date = new Date(cell.getValue() * 1000);
                return date.toLocaleTimeString();
            }
        },
        {title:"DEVADDR", field:"devaddr", sorter:"string", hozAlign:"left", headerFilter:"input", width: 150, tooltip: CLICK_TO_FILTER_STRING},
        {title:"FNCT", field:"fcnt", sorter:"number", hozAlign:"left"},
        {title:"FPORT", field:"fport", sorter:"number", hozAlign:"left", width: 100},
        {title:"SIZE", field:"size", sorter:"number", hozAlign:"left"},
        {title:"TOA", field:"toa", sorter:"number", hozAlign:"left"},
        {title:"FREQ", field:"frequency", sorter:"number", hozAlign:"left", headerFilter:"input", width: 150, tooltip: CLICK_TO_FILTER_STRING},
        {title:"SF", field:"sf", sorter:"number", hozAlign:"left", headerFilter:"input", tooltip: CLICK_TO_FILTER_STRING},
        {title:"BW", field:"bw", sorter:"number", hozAlign:"left", headerFilter:"input", tooltip: CLICK_TO_FILTER_STRING},
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
        {title:"TYPE", field:"mtype", sorter:"string", hozAlign:"left", width:300, headerFilter:"input", tooltip: CLICK_TO_FILTER_STRING, 
            formatter: function(cell, formatterParams, onRendered) {
                return LORAWAN_MESSAGE_TYPES[parseInt(cell.getValue(), 10)];
            }
        },
        {title:"ADR", field:"adr", sorter:"string", hozAlign:"left"},
        {title:"ACK", field:"ack", sorter:"string", hozAlign:"left"},
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

setInterval( function() { uplinks_table.replaceData(); }, 1000 );