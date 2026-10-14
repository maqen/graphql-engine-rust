# Vendored from hasura/graphql-engine server/tests-py/context.py (v2.51.0):
# ActionsWebhookHandler, the action handler of test_actions.py, reading the engine's URL from
# $HGE_CONFORMANCE_ENGINE. Served by a ThreadingHTTPServer (upstream's is single-threaded), so a
# handler that calls back into the engine cannot block other requests.
# Run as: python3 actions_webhook.py <port>

from collections import OrderedDict
from http import HTTPStatus
import http.server
import json
import os
import re
import sys
import time

import requests


class ActionsWebhookHandler(http.server.BaseHTTPRequestHandler):
    # The engine to call back: the runner writes its URL and admin secret to the file named by
    # $HGE_CONFORMANCE_ENGINE whenever it starts one (upstream passes them in at start-up).
    @property
    def _engine(self):
        with open(os.environ['HGE_CONFORMANCE_ENGINE']) as f:
            return json.load(f)

    @property
    def hge_url(self):
        return self._engine['url']

    @property
    def hge_key(self):
        return self._engine['admin_secret']

    def do_GET(self):
        self.send_response(HTTPStatus.OK)
        self.end_headers()

    def do_POST(self):
        content_len = self.headers.get('Content-Length')
        req_body = self.rfile.read(int(content_len)).decode("utf-8")
        self.req_json = json.loads(req_body)
        req_headers = self.headers
        req_path = self.path
        self.log_message(json.dumps(self.req_json))

        if req_path == "/create-user":
            resp, status = self.create_user()
            self._send_response(status, resp)

        elif req_path == "/create-user-timeout":
            time.sleep(3)
            resp, status = self.create_user()
            self._send_response(status, resp)

        elif req_path == "/create-users":
            resp, status = self.create_users()
            self._send_response(status, resp)

        elif req_path == "/create-user-nested":
            resp, status = self.create_user_nested()
            self._send_response(status, resp)

        elif req_path == "/mirror-action":
            resp, status = self.mirror_action()
            self._send_response(status, resp)

        elif req_path == "/mirror-headers":
            resp, status = self.mirror_headers()
            self._send_response(status, resp)

        elif req_path == "/get-user-by-email":
            resp, status = self.get_users_by_email(True)
            self._send_response(status, resp)

        elif req_path == "/get-user-by-email-nested":
            resp, status = self.get_users_by_email_nested(True)
            self._send_response(status, resp)

        elif req_path == "/get-users-by-email":
            resp, status = self.get_users_by_email(False)
            self._send_response(status, resp)

        elif req_path == "/get-users-by-email-nested":
            resp, status = self.get_users_by_email_nested(False)
            self._send_response(status, resp)

        elif req_path == "/intentional-error":
            resp, status = self.intentional_error()
            self._send_response(status, resp)

        elif req_path == "/null-response":
            resp, status = self.null_response()
            self._send_response(status, resp)

        elif req_path == "/omitted-response-field":
            self._send_response(
                HTTPStatus.OK,
                self.get_omitted_response_field()
            )

        elif req_path == "/scalar-response":
            self._send_response(HTTPStatus.OK, "some-string")

        elif req_path == "/json-response":
            resp, status = self.json_response()
            self._send_response(status, resp)

        elif req_path == "/custom-scalar-array-response":
            resp, status = self.custom_scalar_array_response()
            self._send_response(status, resp)

        elif req_path == "/scalar-array-response":
            self._send_response(HTTPStatus.OK, ["foo", "bar", None])

        elif req_path == "/recursive-output":
            resp, status = self.recursive_output()
            self._send_response(status, resp)

        elif req_path == "/get-results":
            resp, status = self.get_results()
            self._send_response(status, resp)

        elif req_path == "/typed-nested-null":
            self._send_response(
                HTTPStatus.OK,
                self.get_typed_nested_null()
            )

        elif req_path == "/typed-nested-null-wrong-field":
            self._send_response(
                HTTPStatus.OK,
                self.get_typed_nested_null_wrong_field()
            )

        elif req_path == "/get_messages":
            resp, status = self.get_messages()
            self._send_response(status, resp)

        else:
            self.send_response(HTTPStatus.NO_CONTENT)
            self.end_headers()

    def intentional_error(self):
        blob = self.req_json['input']['blob']
        return blob, HTTPStatus.BAD_REQUEST

    def create_user(self):
        email_address = self.req_json['input']['email']
        name = self.req_json['input']['name']

        if not self.check_email(email_address):
            response = {
                'message': 'Given email address is not valid',
                'code': 'invalid-email'
            }
            return response, HTTPStatus.BAD_REQUEST

        gql_query = '''
        mutation ($email: String! $name: String!) {
          insert_user_one(object: {email: $email, name: $name}){
            id
          }
        }
        '''
        query = {
            'query': gql_query,
            'variables': {
                'email': email_address,
                'name': name
            }
        }
        code, resp = self.execute_query(query)
        if code != 200 or 'data' not in resp:
            response = {
                'message': 'GraphQL query execution failed',
                'code': 'unexpected'
            }
            return response, HTTPStatus.BAD_REQUEST

        response = resp['data']['insert_user_one']

        return response, HTTPStatus.OK

    def create_users(self):
        inputs = self.req_json['input']['users']
        for input in inputs:
            email_address = input['email']
            if not self.check_email(email_address):
                response = {
                    'message': 'Email address is not valid: ' + email_address,
                    'code': 'invalid-email'
                }
                return response, HTTPStatus.BAD_REQUEST

        gql_query = '''
        mutation ($insert_inputs: [user_insert_input!]!){
          insert_user(objects: $insert_inputs){
            returning{
              id
            }
          }
        }
        '''
        query = {
            'query': gql_query,
            'variables': {
                'insert_inputs': inputs
            }
        }
        code, resp = self.execute_query(query)
        if code != 200 or 'data' not in resp:
            response = {
                'message': 'GraphQL query execution failed',
                'code': 'unexpected'
            }
            return response, HTTPStatus.BAD_REQUEST

        response = resp['data']['insert_user']['returning']
        return response, HTTPStatus.OK

    def create_user_nested(self):
        email_address = self.req_json['input']['email']
        name = self.req_json['input']['name']

        if not self.check_email(email_address):
            response = {
                'message': 'Given email address is not valid',
                'code': 'invalid-email'
            }
            return response, HTTPStatus.BAD_REQUEST

        gql_query = '''
        mutation ($email: String! $name: String!) {
          insert_user_one(object: {email: $email, name: $name}){
            id
          }
        }
        '''
        query = {
            'query': gql_query,
            'variables': {
                'email': email_address,
                'name': name
            }
        }
        code, resp = self.execute_query(query)
        if code != 200 or 'data' not in resp:
            response = {
                'message': 'GraphQL query execution failed',
                'code': 'unexpected'
            }
            return response, HTTPStatus.BAD_REQUEST

        response = {
            'userObj': resp['data']['insert_user_one']
        }
        return response, HTTPStatus.OK

    def get_messages(self):
        response = [
            { "content": "baz", "user_name": "foo"},
            { "content": "foo", "user_name": "bar"}
        ]
        return response, HTTPStatus.OK

    def mirror_action(self):
        response = self.req_json['input']['arg']
        return response, HTTPStatus.OK

    def mirror_headers(self):
        response = {
            'headers': list(map(lambda header: { 'name': header[0], 'value': header[1] }, self.headers.items()))
        }
        return response, HTTPStatus.OK

    def get_users_by_email(self, singleUser = False):
        email = self.req_json['input']['email']
        if not self.check_email(email):
            response = {
                'message': 'Given email address is not valid',
                'code': 'invalid-email'
            }
            return response, HTTPStatus.BAD_REQUEST
        gql_query = '''
        query get_user($email:String!) {
           user(where:{email:{_eq:$email}},order_by: {id: asc}) {
            id
        }
        }
        '''
        query = {
            'query': gql_query,
            'variables':{
                'email':email
            }
        }
        code,resp = self.execute_query(query)
        if code != 200 or 'data' not in resp:
            response = {
                'message': 'GraphQL query execution failed',
                'code': 'unexpected'
            }
            return response, HTTPStatus.BAD_REQUEST
        if singleUser:
            return resp['data']['user'][0], HTTPStatus.OK
        else:
            return resp['data']['user'], HTTPStatus.OK

    def get_users_by_email_nested(self, singleUser = False):
        resp, status = self.get_users_by_email(singleUser)
        def make_nested_out_object(outObj):
            address = { 'city': 'New York', 'country': 'USA'}
            outObj['address'] = address
            addresses = [{'city': 'Bangalore', 'country': 'India'}, {'city': 'Melbourne', 'country': 'Australia'}]
            outObj['addresses'] = addresses
            outObj['user_id'] = { 'id': outObj['id']}
            return outObj

        if status != HTTPStatus.OK:
            return resp, status
        if singleUser:
            return make_nested_out_object(resp), status
        else:
            return map(make_nested_out_object, resp), status

    def get_typed_nested_null(self):
        return {
            'id': 1,
            'child': None
        }

    def get_omitted_response_field(self):
        return {
            'country': 'India'
        }

    def get_typed_nested_null_wrong_field(self):
        return {
            'id': None,
            'child': None
        }

    def null_response(self):
        response = None
        return response, HTTPStatus.OK

    def json_response(self):
        response = {
            'foo': 'bar'
        }
        return response, HTTPStatus.OK

    def custom_scalar_array_response(self):
        response = [{
            'foo': 'bar'
        }]
        return response, HTTPStatus.OK

    def recursive_output(self):
        return {
            'direct': {'id': 1, 'this': {'id': 2, 'this': {'id': 3 }}},
            'list': {'id': 1, 'these': [{'id': 2, 'these': [{'id': 3}]}, {'id': 4}]},
            'mutual': {'id': 1, 'that': {'id': 2, 'other': {'id': 3, 'that': {'id': 4}}}}
        }, HTTPStatus.OK

    def get_results(self):
        return {
            'result_ids': [1,2,3,4]
        }, HTTPStatus.OK

    def check_email(self, email):
        regex = '^\\w+([\\.-]?\\w+)*@\\w+([\\.-]?\\w+)*(\\.\\w{2,3})+$'
        return re.search(regex,email)

    def execute_query(self, query):
        headers = {}
        admin_secret = self.hge_key
        if admin_secret is not None:
            headers['X-Hasura-Admin-Secret'] = admin_secret
        resp = requests.post(
            self.hge_url + '/v1/graphql',
            json=query,
            headers=headers,
            timeout=60,
        )
        data = resp.json(object_pairs_hook=OrderedDict)
        self.log_message(json.dumps(data))
        return resp.status_code, data

    def _send_response(self, status, body):
        self.log_request(status)
        self.send_response_only(status)
        self.send_header('Content-Type', 'application/json')
        self.send_header('Set-Cookie', 'abcd')
        self.end_headers()
        self.wfile.write(json.dumps(body).encode("utf-8"))


if __name__ == '__main__':
    server = http.server.ThreadingHTTPServer(('127.0.0.1', int(sys.argv[1])), ActionsWebhookHandler)
    server.serve_forever()
