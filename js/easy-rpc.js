
let msgpack = MessagePack

const REQUEST = 0
const RESPONSE = 1
const NOTIFY = 2

class EasySession {
    constructor(url, service) {
        let socket = new WebSocket(url)
        socket.session = this 
        this.socket = socket
        this.service = service
        this._id = 0
        this._callback = {}
        let self = this 
        return new Promise((resolve, reject) => {
            socket.onmessage = e => self._handle(e.data)
            socket.onopen = e => resolve(self)
            socket.onerror = e => reject(e)
            socket.onclose = e => self.onclose(e)
        });
    }

    onclose(event) {
        if (typeof this.service.__onclose == 'function')
            this.service.__onclose(event)
    }

    next_id() { this._id += 1; return this._id }

    request(method, arg) {
        let id = this.next_id()
        this._send_pack([REQUEST, id, method, arg])
        return new Promise((resolve, reject) => {
            this._callback[id] = {resolve, reject}
        })
    }

    notify(method, arg) { this._send_pack([NOTIFY, method, arg]) }

    close() { this.socket.close(); }

    async _handle(blob) {
        // let data = await blob.arrayBuffer();
        let data = await new Response(blob).arrayBuffer()
        let pack = msgpack.decode(data)
        switch (pack[0]) {
            case REQUEST: {
                let req_id = pack[1]
                let method = pack[2]
                try {
                    let ret = this.service[method](pack[3], this)
                    this._send_pack([RESPONSE, req_id, null, ret])
                } catch (err) {
                    this._send_pack([RESPONSE, req_id, err.toString(), null]);
                }
                break
            }
            case NOTIFY: {
                let method = pack[1];
                try {
                    this.service[method](pack[2], this)
                } catch (err) {
                    console.warn('[easy-rpc]', err.toString())
                }
                break
            }
            case RESPONSE: {
                let req_id = pack[1]
                let err = pack[2]
                let callback = this._callback[req_id]
                if (err != null)
                    callback.reject(err)
                else
                    callback.resolve(pack[3])
                delete this._callback[req_id]
                break
            }
        }
    }

    _send_pack(pack) { this.socket.send(msgpack.encode(pack)) }
}